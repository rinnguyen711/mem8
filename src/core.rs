use crate::embed::Embed;
use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery, VectorQuery};
use crate::scope::detect_scope;
use crate::store::Store;
use chrono::Utc;
use std::sync::Arc;
use uuid::Uuid;

pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 50;

/// Word overlap above which `add` revises an existing memory instead of storing
/// a new one.
///
/// Deliberately near-identical. Overlap is measured on words, so it recognises
/// the same sentence saved twice but not the same idea worded differently:
/// measured against real data, two memories both recording the choice of the
/// porter tokenizer scored 0.14, indistinguishable from unrelated pairs. A
/// threshold low enough to catch that would also merge memories that merely
/// share vocabulary, and merging discards the older content. Catching literal
/// re-saves is the part that can be done safely without semantic similarity.
pub const DUPLICATE_THRESHOLD: f64 = 0.8;

/// Record a search that found nothing, for working out what recall is missing.
///
/// Keyword search fails in two distinguishable ways: the query used words that
/// are simply absent, or it used a synonym of words that are present. Only the
/// second is an argument for semantic search, and the difference is visible only
/// in real queries. Logging both, with the sanitized form beside the original,
/// is what turns that question into evidence.
///
/// Writes to `~/.mem8/missed-searches.log` and never leaves the machine. Any
/// failure is ignored: a search must not break because a log file could not be
/// written. Set `MEM8_NO_MISS_LOG` to turn it off.
fn log_missed_search(raw: &str, sanitized: &str, project: &str) {
    use std::io::Write;

    if std::env::var_os("MEM8_NO_MISS_LOG").is_some() {
        return;
    }

    let Some(home) = dirs::home_dir() else { return };
    let dir = home.join(".mem8");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let line = format!(
        "{}\t{}\t{}\t{}\n",
        chrono::Utc::now().to_rfc3339(),
        project,
        raw.replace(['\t', '\n'], " "),
        sanitized.replace(['\t', '\n'], " "),
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("missed-searches.log"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Fraction of words two texts share, ignoring order, case, and punctuation.
///
/// Returns 0.0 when either side has no words, so an empty string is never a
/// duplicate of anything.
fn word_overlap(a: &str, b: &str) -> f64 {
    fn words(s: &str) -> std::collections::HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect()
    }

    let (a, b) = (words(a), words(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let union = a.union(&b).count();
    if union == 0 {
        return 0.0;
    }
    a.intersection(&b).count() as f64 / union as f64
}

/// Sanitize a raw agent query into a safe FTS query string.
///
/// FTS5 and `plainto_tsquery` both parse their input, and unbalanced quotes or
/// stray operators are errors rather than literal text. Splitting on FTS
/// syntax characters (as a naive sanitizer would) also breaks hyphenated
/// identifiers like `auth-token` into two independent terms, which can then
/// match a document containing "auth" and "token" nowhere near each other.
///
/// Instead, each surviving term is wrapped as a double-quoted FTS5 phrase, so
/// `auth-token login` becomes `"auth-token" "login"`. FTS5 treats a quoted
/// phrase as literal text, hyphen included, so this both avoids the parse
/// error a raw hyphen triggers and searches for the identifier as a unit.
/// Postgres's `plainto_tsquery` receives the same string; it ignores
/// punctuation and operators entirely, so the quotes are inert there.
///
/// A double quote embedded in a term is escaped by doubling it (`""`), which
/// is how FTS5 string literals escape an embedded quote, so no user input can
/// produce a malformed query.
///
/// Common English function words are dropped as well. Both engines require
/// every term to match, so a question phrased naturally — "why do the backends
/// stem the same?" — otherwise finds nothing, because the stored memory is
/// unlikely to contain "why" and "do" and "the". Stripping them leaves the
/// words that carry the meaning. They are only stripped when something else
/// survives, so a search for a phrase made entirely of them still runs.
pub fn sanitize_fts_query(raw: &str) -> Result<String> {
    let cleaned: Vec<&str> = raw
        .split_whitespace()
        .filter(|t| !matches!(*t, "AND" | "OR" | "NOT" | "NEAR"))
        .map(|t| t.trim_matches(|c| "\"'()*:^?!,.;".contains(c)))
        .filter(|t| !t.is_empty())
        .collect();

    let meaningful: Vec<&str> = cleaned
        .iter()
        .copied()
        .filter(|t| !is_stopword(t))
        .collect();

    // Fall back to the full set when a query is nothing but function words, so
    // it still searches rather than erroring.
    let kept = if meaningful.is_empty() {
        &cleaned
    } else {
        &meaningful
    };

    let terms: Vec<String> = kept
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();

    if terms.is_empty() {
        return Err(Mem8Error::InvalidInput(format!(
            "query '{raw}' contains no searchable terms"
        )));
    }

    Ok(terms.join(" "))
}

/// English function words that carry no search signal.
///
/// Deliberately short: it covers the words that turn a natural question into a
/// query matching nothing, and stops well short of a full stopword list, since
/// every word removed here is one a user can no longer search for in a phrase.
fn is_stopword(term: &str) -> bool {
    const STOPWORDS: &[&str] = &[
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "did", "do", "does", "for", "from",
        "had", "has", "have", "how", "i", "in", "is", "it", "of", "on", "or", "our", "so", "than",
        "that", "the", "their", "them", "then", "there", "these", "they", "this", "to", "was",
        "we", "were", "what", "when", "where", "which", "who", "why", "will", "with", "would",
        "you", "your",
    ];
    let lower = term.to_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

/// Rank-fusion constant from the Reciprocal Rank Fusion literature.
///
/// Damps the difference between adjacent ranks, so first place does not
/// overwhelm second. 60 is the standard value and needs no tuning at this
/// scale.
const RRF_K: f64 = 60.0;

/// Merge two ranked lists into one, by rank rather than by score.
///
/// Keyword and vector searches return incomparable numbers — BM25, `ts_rank`,
/// and cosine similarity are three different scales with three different
/// ranges. Their *orderings* are comparable, so fusion uses position:
///
/// ```text
/// score(m) = Σ 1 / (RRF_K + rank_in_list)
/// ```
///
/// A memory found by both searches scores higher than one found by either
/// alone, which is the desired behaviour: agreement between two independent
/// methods is the strongest signal available.
fn reciprocal_rank_fusion(lists: &[Vec<SearchHit>], limit: usize) -> Vec<SearchHit> {
    use std::collections::HashMap;

    let mut fused: HashMap<Uuid, (f64, Memory)> = HashMap::new();

    for list in lists {
        for (rank, hit) in list.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            fused
                .entry(hit.memory.id)
                .and_modify(|(score, _)| *score += contribution)
                .or_insert((contribution, hit.memory.clone()));
        }
    }

    let mut hits: Vec<SearchHit> = fused
        .into_values()
        .map(|(score, memory)| SearchHit { memory, score })
        .collect();

    // Ties broken by recency: with two memories equally ranked, the one
    // revised more recently is the better guess. Without a tiebreak the order
    // would come from HashMap iteration, which varies run to run.
    hits.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.memory.updated_at.cmp(&a.memory.updated_at))
    });
    hits.truncate(limit);
    hits
}

/// How a memory's project is decided when the caller does not name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeMode {
    /// Infer it from the process's working directory.
    ///
    /// Correct when mem8 runs as a child of the agent: the working directory is
    /// the project being worked on. This is stdio mode and every CLI
    /// subcommand.
    Detect,

    /// Require the caller to name it; refuse the call otherwise.
    ///
    /// Correct when mem8 serves over HTTP. The server's working directory is
    /// its own install location — `/app` in a container — and is the same for
    /// every client, so inferring from it would file every project's memories
    /// under one name. There is no safe default here: a wrong project is a
    /// silent misfile, while a refusal is something the caller can see and fix.
    Explicit,
}

/// The memory service. Owns validation and scope resolution so that the MCP
/// server and the CLI behave identically.
pub struct Memory8 {
    store: Arc<dyn Store>,
    /// Present only when semantic search is configured. `None` is the normal
    /// case: keyword-only, exactly as before this feature existed.
    embedder: Option<Arc<dyn Embed>>,
    scope_mode: ScopeMode,
}

impl Memory8 {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            embedder: None,
            scope_mode: ScopeMode::Detect,
        }
    }

    /// Enable semantic search, embedding on write and searching by vector
    /// alongside keywords.
    pub fn with_embedder(store: Arc<dyn Store>, embedder: Arc<dyn Embed>) -> Self {
        Self {
            store,
            embedder: Some(embedder),
            scope_mode: ScopeMode::Detect,
        }
    }

    /// Require callers to name their project rather than inferring it.
    ///
    /// For serving over HTTP, where the process's working directory says
    /// nothing about who is asking.
    pub fn with_scope_mode(mut self, mode: ScopeMode) -> Self {
        self.scope_mode = mode;
        self
    }

    /// Embed text, or `None` if there is no embedder or it failed.
    ///
    /// Failure is deliberately not an error. A memory stored without an
    /// embedding is findable by keyword and can be backfilled later by
    /// `mem8 reindex`; a memory refused because a similarity index was
    /// unavailable is simply lost. The first outcome is recoverable and the
    /// second is not.
    fn try_embed(&self, text: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        match embedder.embed_one(text) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("mem8: embedding failed, storing without one: {e}");
                None
            }
        }
    }

    /// The project a call applies to.
    ///
    /// An explicit name always wins. Without one, the answer depends on the
    /// mode: detect it locally, or refuse remotely, where there is nothing
    /// trustworthy to detect it from.
    fn resolve_scope(&self, explicit: Option<String>) -> Result<String> {
        if let Some(p) = explicit {
            if !p.trim().is_empty() {
                return Ok(p.trim().to_string());
            }
        }

        match self.scope_mode {
            ScopeMode::Detect => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                Ok(detect_scope(&cwd))
            }
            ScopeMode::Explicit => Err(Mem8Error::InvalidInput(
                "'project' is required when mem8 serves over HTTP: the server cannot \
                 infer which project you mean, because its working directory is its own \
                 rather than yours. Pass 'project' explicitly, or use 'global: true' to \
                 search across all of them."
                    .into(),
            )),
        }
    }

    /// Store a memory, optionally recording that it replaces an existing one.
    ///
    /// `supersedes` names a memory in the same project that this one makes
    /// obsolete. The old memory is invalidated rather than deleted: search stops
    /// returning it, but `get` still does, which is what keeps "we used SQLite
    /// until 2026-08-17" answerable.
    pub async fn add(
        &self,
        content: &str,
        kind: Kind,
        tags: Vec<String>,
        project: Option<String>,
        supersedes: Option<Uuid>,
    ) -> Result<Memory> {
        if content.trim().is_empty() {
            return Err(Mem8Error::InvalidInput("content must not be empty".into()));
        }

        let content = content.trim().to_string();
        let project = self.resolve_scope(project)?;

        // Validate before embedding. Every check runs before anything is
        // written, so a rejected supersede costs neither a model pass nor a
        // partial write.
        if let Some(old_id) = supersedes {
            let target = self.store.get(old_id).await?;

            if target.project != project {
                return Err(Mem8Error::InvalidInput(format!(
                    "cannot supersede memory {old_id}: it belongs to project '{}', \
                     but this memory is being written to '{}'. A cross-project \
                     supersession is far more likely a mistaken id than an intent.",
                    target.project, project
                )));
            }

            // Keyed on `invalid_at`, not `superseded_by`. A memory can be dead
            // with an unknown successor -- markdown import records the
            // invalidation without always knowing which memory replaced it --
            // and such a row must be refused here too. Checking
            // `superseded_by` alone would let it through and then fail deeper
            // in the store's write-once guard with a message that says nothing
            // about what the caller should do instead.
            if let Some(invalid_at) = target.invalid_at {
                return Err(Mem8Error::InvalidInput(match target.superseded_by {
                    Some(existing) => format!(
                        "memory {old_id} is already superseded by {existing}; \
                         supersede that one instead so the chain stays linear"
                    ),
                    None => format!(
                        "memory {old_id} was already invalidated as of {}, with no \
                         recorded successor; it is no longer current, so there is \
                         nothing left for this memory to replace",
                        invalid_at.to_rfc3339()
                    ),
                }));
            }
        }

        let embedding = self.try_embed(&content);

        // Revise a near-identical memory rather than storing it twice. Only the
        // resolved project is considered, so this can never merge across scopes.
        //
        // An explicit `supersedes` skips duplicate detection entirely: the
        // agent has already stated what this memory replaces, and re-deriving
        // it by word overlap can only disagree with a direct answer.
        if supersedes.is_none() {
            if let Some(existing) = self.find_duplicate(&content, &project).await {
                return self
                    .store
                    .update(
                        existing.id,
                        MemoryUpdate {
                            content: Some(content),
                            kind: Some(kind),
                            tags: Some(tags),
                            embedding,
                        },
                    )
                    .await;
            }
        }

        // Write the new memory first, then invalidate the old one pointing at
        // it. The reverse order leaves a window in which the old fact is dead
        // and nothing has replaced it, and a crash inside that window loses the
        // fact entirely. In this order a crash after the first step leaves two
        // live memories -- the condition that exists today, which the next
        // write can still repair.
        let created = self
            .store
            .add(NewMemory {
                project,
                kind,
                content,
                tags,
                embedding,
            })
            .await?;

        if let Some(old_id) = supersedes {
            self.store
                .supersede(old_id, Some(created.id), Utc::now())
                .await?;
        }

        Ok(created)
    }

    /// The stored memory this content would duplicate, if any.
    ///
    /// Searches for the content's own terms and compares each candidate by word
    /// overlap. A failed search is not an error here: duplicate detection is an
    /// optimisation, and a query that cannot be parsed simply means the write
    /// proceeds as a new memory.
    async fn find_duplicate(&self, content: &str, project: &str) -> Option<Memory> {
        let text = sanitize_fts_query(content).ok()?;

        let hits = self
            .store
            .search(SearchQuery {
                text,
                project: Some(project.to_string()),
                global: false,
                kind: None,
                tags: Vec::new(),
                limit: MAX_LIMIT,
                // Never merge a new write into a memory that is already dead.
                include_superseded: false,
                as_of: None,
            })
            .await
            .ok()?;

        hits.into_iter()
            .map(|hit| (word_overlap(content, &hit.memory.content), hit.memory))
            .filter(|(overlap, _)| *overlap >= DUPLICATE_THRESHOLD)
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(_, memory)| memory)
    }

    /// Fetch a memory by id, superseded or not.
    ///
    /// Never filters on `invalid_at`. A replaced memory is hidden from search
    /// but still readable here, which is what keeps "we used SQLite until
    /// 2026-08-17" answerable.
    pub async fn get(&self, id: Uuid) -> Result<Memory> {
        self.store.get(id).await
    }

    pub async fn update(
        &self,
        id: Uuid,
        content: Option<String>,
        kind: Option<Kind>,
        tags: Option<Vec<String>>,
    ) -> Result<Memory> {
        // Trim here as `add` does, so a memory's stored content does not depend
        // on which call wrote it.
        let content = match content {
            Some(c) if c.trim().is_empty() => {
                return Err(Mem8Error::InvalidInput("content must not be empty".into()));
            }
            Some(c) => Some(c.trim().to_string()),
            None => None,
        };

        // Re-embed only when the content changed. An update that touches just
        // tags or kind leaves the stored vector correct, and recomputing it
        // would spend a model pass to arrive at the same answer.
        let embedding = content.as_deref().and_then(|c| self.try_embed(c));

        self.store
            .update(
                id,
                MemoryUpdate {
                    content,
                    kind,
                    tags,
                    embedding,
                },
            )
            .await
    }

    pub async fn delete(&self, id: Uuid) -> Result<()> {
        self.store.delete(id).await
    }

    pub async fn search(
        &self,
        query: &str,
        project: Option<String>,
        global: bool,
        kind: Option<Kind>,
        tags: Vec<String>,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Err(Mem8Error::InvalidInput("query must not be empty".into()));
        }

        let text = sanitize_fts_query(query)?;
        let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        // A global search deliberately crosses every project, so there is
        // nothing to resolve and nothing to refuse. It is explicit about its
        // own breadth, which is the opposite of the accidental cross-project
        // write that `Explicit` mode exists to prevent.
        let project = if global {
            None
        } else {
            Some(self.resolve_scope(project)?)
        };
        let scope = project.clone().unwrap_or_else(|| "*".to_string());

        let keyword = self
            .store
            .search(SearchQuery {
                text: text.clone(),
                project: project.clone(),
                global,
                kind,
                tags: tags.clone(),
                limit,
                include_superseded: false,
                as_of: None,
            })
            .await?;

        // Semantic search runs alongside, never instead. Keyword search is what
        // finds exact identifiers -- `SqliteStore`, `auth-token`, a commit hash
        // -- which embeddings match poorly, so replacing it would trade one
        // class of failure for another.
        let hits = match self
            .vector_hits(query, project, global, kind, &tags, limit)
            .await
        {
            Some(vector) if !vector.is_empty() => reciprocal_rank_fusion(&[keyword, vector], limit),
            _ => keyword,
        };

        if hits.is_empty() {
            log_missed_search(query, &text, &scope);
        }

        Ok(hits)
    }

    /// Vector hits for a query, or `None` when semantic search is unavailable.
    ///
    /// Every failure here degrades to keyword-only rather than propagating: no
    /// embedder configured, a backend that does not implement vector search, an
    /// embedding that could not be computed, or a store error. Search working
    /// less well is a far better outcome than search not working, and the
    /// SQLite default hits the `Unsupported` path on every single query.
    async fn vector_hits(
        &self,
        query: &str,
        project: Option<String>,
        global: bool,
        kind: Option<Kind>,
        tags: &[String],
        limit: usize,
    ) -> Option<Vec<SearchHit>> {
        let embedding = self.try_embed(query)?;

        match self
            .store
            .vector_search(VectorQuery {
                embedding,
                project,
                global,
                kind,
                tags: tags.to_vec(),
                limit,
                include_superseded: false,
                as_of: None,
            })
            .await
        {
            Ok(hits) => Some(hits),
            Err(Mem8Error::Unsupported { .. }) => None,
            Err(e) => {
                eprintln!("mem8: semantic search failed, falling back to keywords: {e}");
                None
            }
        }
    }

    /// Every memory, for `mem8 export`.
    pub async fn all(&self) -> Result<Vec<Memory>> {
        self.store.all().await
    }

    /// Embed memories that have no embedding. Returns how many were backfilled.
    ///
    /// Memories written before semantic search was enabled are invisible to
    /// vector search until this runs; it is the only path by which an existing
    /// database gains it.
    ///
    /// Idempotent — a second run finds nothing to do, because it selects only
    /// rows where the embedding is NULL.
    pub async fn reindex(&self, batch_size: usize) -> Result<usize> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Err(Mem8Error::InvalidInput(
                "no embedding model is configured; build mem8 with the `semantic` feature".into(),
            ));
        };

        let mut total = 0;
        loop {
            let batch = self.store.missing_embeddings(batch_size).await?;
            if batch.is_empty() {
                break;
            }

            let texts: Vec<&str> = batch.iter().map(|m| m.content.as_str()).collect();
            let vectors = embedder.embed_batch(&texts)?;

            if vectors.len() != batch.len() {
                return Err(Mem8Error::Store(format!(
                    "embedder returned {} vectors for {} memories",
                    vectors.len(),
                    batch.len()
                )));
            }

            for (memory, vector) in batch.iter().zip(vectors.iter()) {
                self.store.set_embedding(memory.id, vector).await?;
                total += 1;
            }

            // A short batch means the store had no more to give.
            if batch.len() < batch_size {
                break;
            }
        }

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;
    use crate::store::MemStore;
    use std::sync::Arc;

    fn service() -> Memory8 {
        Memory8::new(Arc::new(MemStore::new()))
    }

    #[tokio::test]
    async fn add_rejects_empty_content() {
        let svc = service();
        let err = svc
            .add("   ", Kind::Fact, vec![], Some("p1".into()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(err.to_string().contains("content"));
    }

    #[tokio::test]
    async fn add_uses_explicit_project_over_detection() {
        let svc = service();
        let m = svc
            .add("a fact", Kind::Fact, vec![], Some("explicit".into()), None)
            .await
            .unwrap();
        assert_eq!(m.project, "explicit");
    }

    #[tokio::test]
    async fn add_falls_back_to_detected_scope() {
        let svc = service();
        let m = svc
            .add("a fact", Kind::Fact, vec![], None, None)
            .await
            .unwrap();
        assert!(!m.project.is_empty(), "detected scope must never be empty");
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let svc = service();
        let err = svc
            .search("", None, false, None, vec![], None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn search_clamps_limit_to_maximum() {
        let svc = service();
        for i in 0..60 {
            svc.add(
                &format!("fact number {i}"),
                Kind::Fact,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        }
        let hits = svc
            .search("fact", Some("p1".into()), false, None, vec![], Some(999))
            .await
            .unwrap();
        assert!(
            hits.len() <= MAX_LIMIT,
            "limit must be clamped to {MAX_LIMIT}"
        );
    }

    #[tokio::test]
    async fn search_defaults_limit_to_ten() {
        let svc = service();
        for i in 0..20 {
            svc.add(
                &format!("fact number {i}"),
                Kind::Fact,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        }
        let hits = svc
            .search("fact", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert_eq!(hits.len(), DEFAULT_LIMIT);
    }

    #[test]
    fn sanitize_strips_unbalanced_quotes() {
        // An unbalanced quote in the raw query must not carry through into an
        // unbalanced quote in the emitted FTS string -- that would make the
        // query malformed. The term's own stray `"` is trimmed, then the term
        // is re-wrapped in a fresh, properly balanced pair of quotes.
        let cleaned = sanitize_fts_query("auth \"broken").unwrap();
        assert_eq!(cleaned, "\"auth\" \"broken\"");
        assert_eq!(cleaned.matches('"').count() % 2, 0, "quotes must balance");
    }

    #[test]
    fn sanitize_strips_fts_operators() {
        // Bare FTS operator keywords are dropped; stray operator punctuation
        // clinging to a term is trimmed before the term is quoted, so no
        // operator syntax survives into the emitted query.
        let cleaned = sanitize_fts_query("auth AND (login OR session)*").unwrap();
        assert_eq!(cleaned, "\"auth\" \"login\" \"session\"");
        assert!(!cleaned.contains('('));
        assert!(!cleaned.contains(')'));
        assert!(!cleaned.contains('*'));
    }

    #[test]
    fn sanitize_drops_function_words_from_a_question() {
        // The exact query that found nothing in real use, against a memory
        // reading "both backends stem identically".
        let cleaned = sanitize_fts_query("why do backends stem the same").unwrap();
        assert!(!cleaned.contains("why"), "got: {cleaned}");
        assert!(!cleaned.contains("\"do\""), "got: {cleaned}");
        assert!(!cleaned.contains("\"the\""), "got: {cleaned}");
        assert!(
            cleaned.contains("backends") && cleaned.contains("stem"),
            "got: {cleaned}"
        );
    }

    #[test]
    fn sanitize_keeps_function_words_when_they_are_all_there_is() {
        // Stripping every term would turn a legitimate search into an error.
        let cleaned = sanitize_fts_query("the who").unwrap();
        assert!(
            cleaned.contains("the") && cleaned.contains("who"),
            "got: {cleaned}"
        );
    }

    #[test]
    fn sanitize_strips_trailing_question_marks() {
        let cleaned = sanitize_fts_query("tokenizer?").unwrap();
        assert_eq!(cleaned, "\"tokenizer\"");
    }

    #[test]
    fn sanitize_rejects_a_query_with_no_usable_terms() {
        assert!(sanitize_fts_query("\"\"()*").is_err());
    }

    #[test]
    fn hyphenated_term_is_quoted_not_split() {
        // The old sanitizer stripped `-` and produced two bare terms
        // (`auth token`), which could match a document containing "auth" and
        // "token" nowhere near each other. It must now survive as one quoted
        // phrase so FTS5 treats the hyphenated identifier as literal text.
        let cleaned = sanitize_fts_query("auth-token").unwrap();
        assert_eq!(cleaned, "\"auth-token\"");
    }

    #[test]
    fn embedded_quote_cannot_break_the_query() {
        // A term containing a literal `"` must not produce a malformed FTS
        // string. The embedded quote is escaped by doubling it, so the
        // overall query stays a well-formed, balanced sequence of phrases.
        let cleaned = sanitize_fts_query("say\"hi").unwrap();
        assert_eq!(cleaned, "\"say\"\"hi\"");
        assert_eq!(cleaned.matches('"').count() % 2, 0, "quotes must balance");
    }

    #[tokio::test]
    async fn adding_the_same_content_twice_revises_one_memory() {
        let svc = service();
        let first = svc
            .add(
                "We chose the porter tokenizer.",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        let second = svc
            .add(
                "We chose the porter tokenizer.",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            second.id, first.id,
            "a re-save should revise, not duplicate"
        );
        assert_eq!(svc.all().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn differently_worded_memories_are_kept_apart() {
        // These two record the same decision but share few words. Merging them
        // would discard content, so the threshold deliberately does not reach
        // this far -- it is the case only semantic similarity can catch.
        let svc = service();
        svc.add(
            "We chose the porter tokenizer so both backends stem identically.",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();
        svc.add(
            "mem8 uses SQLite FTS5 with porter, matching how Postgres stems.",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            svc.all().await.unwrap().len(),
            2,
            "distinct wording must not merge"
        );
    }

    #[tokio::test]
    async fn duplicate_detection_does_not_cross_projects() {
        let svc = service();
        svc.add(
            "Run cargo fmt before committing.",
            Kind::Convention,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();
        svc.add(
            "Run cargo fmt before committing.",
            Kind::Convention,
            vec![],
            Some("p2".into()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            svc.all().await.unwrap().len(),
            2,
            "identical content in different projects is not a duplicate"
        );
    }

    #[tokio::test]
    async fn a_search_that_finds_nothing_still_succeeds() {
        // The miss log is best-effort; a search must return Ok either way.
        let svc = service();
        svc.add(
            "we chose rust",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();

        let hits = svc
            .search("kubernetes", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn word_overlap_ignores_case_order_and_punctuation() {
        assert_eq!(
            word_overlap("Porter tokenizer, chosen.", "chosen porter TOKENIZER"),
            1.0
        );
        assert_eq!(word_overlap("", "anything"), 0.0);
        assert!(word_overlap("we chose porter", "we picked snowball") < 0.5);
    }

    #[tokio::test]
    async fn update_trims_content_like_add_does() {
        let svc = service();
        let added = svc
            .add(
                "  spaced out  ",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(added.content, "spaced out");

        let updated = svc
            .update(added.id, Some("  revised  ".into()), None, None)
            .await
            .unwrap();
        assert_eq!(updated.content, "revised");
    }

    #[tokio::test]
    async fn update_on_missing_id_is_not_found() {
        let svc = service();
        let err = svc
            .update(uuid::Uuid::new_v4(), None, None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::NotFound(_)));
    }

    // ---- scope mode ---------------------------------------------------------

    /// A service that refuses to guess the project, as HTTP mode does.
    fn explicit_service() -> Memory8 {
        Memory8::new(Arc::new(MemStore::new())).with_scope_mode(ScopeMode::Explicit)
    }

    #[tokio::test]
    async fn explicit_mode_refuses_a_write_with_no_project() {
        // Over HTTP the server's working directory belongs to the server, not
        // the caller. Guessing from it would file every client's memories under
        // one name, silently. Refusing is the only honest answer.
        let svc = explicit_service();
        let err = svc
            .add("a fact", Kind::Fact, vec![], None, None)
            .await
            .unwrap_err();

        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        let message = err.to_string();
        assert!(
            message.contains("project"),
            "the error must name the field: {message}"
        );
    }

    #[tokio::test]
    async fn explicit_mode_accepts_a_named_project() {
        let svc = explicit_service();
        let m = svc
            .add("a fact", Kind::Fact, vec![], Some("named".into()), None)
            .await
            .unwrap();
        assert_eq!(m.project, "named");
    }

    #[tokio::test]
    async fn explicit_mode_rejects_a_blank_project_as_if_it_were_missing() {
        // Whitespace is not a project name. Accepting it would create a scope
        // that no one can search for by name.
        let svc = explicit_service();
        let err = svc
            .add("a fact", Kind::Fact, vec![], Some("   ".into()), None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn explicit_mode_refuses_a_scoped_search_with_no_project() {
        let svc = explicit_service();
        let err = svc
            .search("anything", None, false, None, vec![], None)
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn explicit_mode_still_allows_a_global_search() {
        // `global: true` is explicit about crossing projects, which is the
        // opposite of the accidental misfile this mode prevents.
        let svc = explicit_service();
        svc.add(
            "we chose rust",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();

        let hits = svc
            .search("rust", None, true, None, vec![], None)
            .await
            .expect("a global search names no project by design");
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn detect_mode_still_infers_the_project() {
        // stdio is the default and must not regress.
        let svc = service();
        let m = svc
            .add("a fact", Kind::Fact, vec![], None, None)
            .await
            .unwrap();
        assert!(!m.project.is_empty(), "detection must still work locally");
    }

    #[tokio::test]
    async fn explicit_mode_trims_the_project_name() {
        let svc = explicit_service();
        let m = svc
            .add(
                "a fact",
                Kind::Fact,
                vec![],
                Some("  spaced  ".into()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            m.project, "spaced",
            "a stray space must not create a second scope"
        );
    }

    // ---- semantic search ----------------------------------------------------

    use crate::embed::FakeEmbedder;

    /// A service with semantic search enabled, backed by the deterministic
    /// fake embedder rather than a 130 MB model download.
    fn semantic_service() -> Memory8 {
        Memory8::with_embedder(Arc::new(MemStore::new()), Arc::new(FakeEmbedder))
    }

    fn hit(id: Uuid, score: f64) -> SearchHit {
        SearchHit {
            memory: Memory {
                id,
                project: "p1".into(),
                kind: Kind::Fact,
                content: String::new(),
                tags: vec![],
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                embedding: None,
                superseded_by: None,
                invalid_at: None,
            },
            score,
        }
    }

    #[test]
    fn rrf_ranks_a_memory_found_by_both_searches_first() {
        // `both` is second in each list, so neither search alone ranks it top.
        // Agreement between two independent methods is the strongest signal
        // available, so fusion must lift it above either list's own winner.
        let both = Uuid::new_v4();
        let keyword_only = Uuid::new_v4();
        let vector_only = Uuid::new_v4();

        let keyword = vec![hit(keyword_only, 9.9), hit(both, 0.1)];
        let vector = vec![hit(vector_only, 0.99), hit(both, 0.5)];

        let fused = reciprocal_rank_fusion(&[keyword, vector], 10);

        assert_eq!(
            fused[0].memory.id, both,
            "found by both must outrank found by one"
        );
        assert_eq!(
            fused.len(),
            3,
            "every distinct memory should survive the merge"
        );
    }

    #[test]
    fn rrf_ignores_incomparable_score_scales() {
        // BM25 magnitudes dwarf cosine similarity. Fusion uses rank, so a
        // huge keyword score must not by itself decide the order.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        let keyword = vec![hit(a, 1000.0)];
        let vector = vec![hit(b, 0.9), hit(a, 0.8)];

        let fused = reciprocal_rank_fusion(&[keyword, vector], 10);

        // `a` is rank 1 of one list and rank 2 of the other; `b` is rank 1 of
        // one list only. `a` wins on rank alone, despite the scores.
        assert_eq!(fused[0].memory.id, a);
        assert!(
            fused[0].score < 1.0,
            "fused scores are rank-based, not raw: {}",
            fused[0].score
        );
    }

    #[test]
    fn rrf_respects_the_limit() {
        let lists = vec![(0..10)
            .map(|_| hit(Uuid::new_v4(), 1.0))
            .collect::<Vec<_>>()];
        assert_eq!(reciprocal_rank_fusion(&lists, 3).len(), 3);
    }

    #[tokio::test]
    async fn writes_carry_an_embedding_when_one_is_configured() {
        let svc = semantic_service();
        let m = svc
            .add(
                "we chose rust",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        let stored = svc.get(m.id).await.unwrap();
        assert_eq!(
            stored.embedding.map(|e| e.len()),
            Some(crate::embed::EMBEDDING_DIM),
            "an embedder-backed write must store a vector"
        );
    }

    #[tokio::test]
    async fn writes_have_no_embedding_without_an_embedder() {
        let svc = service();
        let m = svc
            .add(
                "we chose rust",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        assert!(svc.get(m.id).await.unwrap().embedding.is_none());
    }

    #[tokio::test]
    async fn search_still_works_when_the_backend_has_no_vector_search() {
        // The SQLite default returns `Unsupported` on every vector search.
        // That must degrade to keyword-only, not surface as an error -- this
        // is the path every default install takes.
        struct NoVectors(MemStore);

        #[async_trait::async_trait]
        impl Store for NoVectors {
            async fn add(&self, new: NewMemory) -> Result<Memory> {
                self.0.add(new).await
            }
            async fn get(&self, id: Uuid) -> Result<Memory> {
                self.0.get(id).await
            }
            async fn update(&self, id: Uuid, u: MemoryUpdate) -> Result<Memory> {
                self.0.update(id, u).await
            }
            async fn delete(&self, id: Uuid) -> Result<()> {
                self.0.delete(id).await
            }
            async fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>> {
                self.0.search(q).await
            }
            async fn all(&self) -> Result<Vec<Memory>> {
                self.0.all().await
            }
            async fn vector_search(&self, _q: VectorQuery) -> Result<Vec<SearchHit>> {
                Err(Mem8Error::Unsupported {
                    feature: "semantic search".into(),
                    backend: "a test backend".into(),
                })
            }
            async fn missing_embeddings(&self, _limit: usize) -> Result<Vec<Memory>> {
                Ok(Vec::new())
            }
            async fn set_embedding(&self, _id: Uuid, _e: &[f32]) -> Result<()> {
                Ok(())
            }
            async fn supersede(
                &self,
                old: Uuid,
                new: Option<Uuid>,
                at: chrono::DateTime<chrono::Utc>,
            ) -> Result<()> {
                self.0.supersede(old, new, at).await
            }
        }

        let svc =
            Memory8::with_embedder(Arc::new(NoVectors(MemStore::new())), Arc::new(FakeEmbedder));
        svc.add(
            "we chose rust",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();

        let hits = svc
            .search("rust", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "an unsupported vector search must not fail the whole search"
        );
    }

    #[tokio::test]
    async fn vector_search_does_not_cross_project_scope() {
        let svc = semantic_service();
        svc.add(
            "we chose rust",
            Kind::Decision,
            vec![],
            Some("p1".into()),
            None,
        )
        .await
        .unwrap();
        svc.add(
            "we chose rust",
            Kind::Decision,
            vec![],
            Some("p2".into()),
            None,
        )
        .await
        .unwrap();

        let hits = svc
            .search("rust", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert!(
            hits.iter().all(|h| h.memory.project == "p1"),
            "semantic search must respect project scope, got: {:?}",
            hits.iter().map(|h| &h.memory.project).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn reindex_backfills_then_finds_nothing_to_do() {
        // Memories written before semantic search was enabled have no
        // embedding. Reindex is the only path by which they gain one.
        let store = Arc::new(MemStore::new());
        let plain = Memory8::new(store.clone());
        for i in 0..3 {
            plain
                .add(
                    &format!("memory number {i}"),
                    Kind::Fact,
                    vec![],
                    Some("p1".into()),
                    None,
                )
                .await
                .unwrap();
        }

        let svc = Memory8::with_embedder(store, Arc::new(FakeEmbedder));
        assert_eq!(svc.reindex(64).await.unwrap(), 3);

        // Idempotent: a second run has nothing left to embed.
        assert_eq!(svc.reindex(64).await.unwrap(), 0);

        for memory in svc.all().await.unwrap() {
            assert!(
                memory.embedding.is_some(),
                "every memory should be embedded after reindex"
            );
        }
    }

    #[tokio::test]
    async fn reindex_pages_through_more_memories_than_one_batch() {
        let store = Arc::new(MemStore::new());
        let plain = Memory8::new(store.clone());
        for i in 0..10 {
            plain
                .add(
                    &format!("memory number {i}"),
                    Kind::Fact,
                    vec![],
                    Some("p1".into()),
                    None,
                )
                .await
                .unwrap();
        }

        let svc = Memory8::with_embedder(store, Arc::new(FakeEmbedder));
        assert_eq!(
            svc.reindex(3).await.unwrap(),
            10,
            "batching must not drop the remainder"
        );
    }

    #[tokio::test]
    async fn reindex_without_an_embedder_is_a_clear_error() {
        let err = service().reindex(64).await.unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(err.to_string().contains("semantic"), "got: {err}");
    }

    #[tokio::test]
    async fn a_failing_embedder_does_not_lose_the_write() {
        // Losing a memory because an optional index was unavailable is a worse
        // outcome than a memory that is temporarily keyword-only.
        struct BrokenEmbedder;
        impl Embed for BrokenEmbedder {
            fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                Err(Mem8Error::Store("model exploded".into()))
            }
        }

        let svc = Memory8::with_embedder(Arc::new(MemStore::new()), Arc::new(BrokenEmbedder));
        let m = svc
            .add(
                "still worth keeping",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .expect("a failing embedder must not fail the write");

        let stored = svc.get(m.id).await.unwrap();
        assert_eq!(stored.content, "still worth keeping");
        assert!(stored.embedding.is_none());

        // And it remains findable the ordinary way.
        let hits = svc
            .search("worth", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
    }

    // --- Task 7: explicit supersession on the write path ---------------------

    #[tokio::test]
    async fn supersedes_hides_the_old_memory_and_links_them() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let old = service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        let new = service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        let fetched = service.get(old.id).await.unwrap();
        assert_eq!(fetched.superseded_by, Some(new.id));
        assert!(fetched.invalid_at.is_some());

        let hits = service
            .search("storage", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.memory.id).collect();
        assert!(!ids.contains(&old.id));
        assert!(ids.contains(&new.id));
    }

    #[tokio::test]
    async fn superseding_a_missing_memory_is_not_found_and_writes_nothing() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let absent = Uuid::new_v4();

        let err = service
            .add(
                "new fact",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(absent),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::NotFound(_)));

        // Nothing was written: validation precedes both writes.
        assert!(service.all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn superseding_across_projects_is_rejected() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let other = service
            .add(
                "other project fact",
                Kind::Decision,
                vec![],
                Some("p2".into()),
                None,
            )
            .await
            .unwrap();

        let err = service
            .add(
                "new fact",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(other.id),
            )
            .await
            .unwrap_err();

        // A cross-project supersession is far more likely a mistaken id than
        // intent.
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        let message = err.to_string();
        assert!(
            message.contains("p1") && message.contains("p2"),
            "got: {message}"
        );
    }

    #[tokio::test]
    async fn superseding_an_already_superseded_memory_is_rejected() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let first = service
            .add("v1", Kind::Decision, vec![], Some("p1".into()), None)
            .await
            .unwrap();
        let second = service
            .add(
                "v2 entirely different words",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(first.id),
            )
            .await
            .unwrap();

        // Chains stay linear: a memory has at most one successor.
        let err = service
            .add(
                "v3 other words again",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(first.id),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(
            err.to_string().contains(&second.id.to_string()),
            "error must name the existing successor, got: {err}"
        );
    }

    #[tokio::test]
    async fn superseding_a_dead_memory_with_no_known_successor_is_rejected() {
        // After markdown import a memory can be dead with an unknown successor:
        // `invalid_at` set, `superseded_by` NULL. Superseding it again must be
        // refused here, with a message of its own, rather than falling through
        // to the store's write-once rejection. This is why the guard keys on
        // `invalid_at` rather than `superseded_by`.
        let store = Arc::new(MemStore::new());
        let service = Memory8::new(store.clone());
        let dead = service
            .add("retired fact", Kind::Fact, vec![], Some("p1".into()), None)
            .await
            .unwrap();
        store.supersede(dead.id, None, Utc::now()).await.unwrap();

        let err = service
            .add(
                "the replacement fact",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                Some(dead.id),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(
            err.to_string().contains("recorded successor"),
            "the message must say the successor is unknown, got: {err}"
        );
        assert_eq!(
            service.all().await.unwrap().len(),
            1,
            "a rejected supersede must write nothing"
        );
    }

    #[tokio::test]
    async fn explicit_supersedes_skips_duplicate_detection() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let old = service
            .add(
                "we use the porter tokenizer",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        // Near-identical content would normally be revised in place. With an
        // explicit supersedes the agent has already said what this replaces, so
        // re-deriving it by word overlap can only disagree with a direct answer.
        let new = service
            .add(
                "we use the porter tokenizer",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        assert_ne!(
            new.id, old.id,
            "must create a new memory, not revise the old one"
        );
        assert_eq!(service.all().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn duplicate_detection_ignores_superseded_memories() {
        let service = Memory8::new(Arc::new(MemStore::new()));
        let old = service
            .add(
                "we use the porter tokenizer",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        let replacement = service
            .add(
                "something else entirely",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();

        // Re-saving the old wording must not merge into the dead memory.
        let fresh = service
            .add(
                "we use the porter tokenizer",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        assert_ne!(
            fresh.id, old.id,
            "a new write must never merge into a dead memory"
        );
        assert_ne!(fresh.id, replacement.id);
    }

    /// The ordering guarantee, proven rather than asserted in a comment.
    ///
    /// If invalidating the old memory fails, the surviving state must be the
    /// recoverable one: the new memory present, the old one still live. The
    /// reverse -- old dead, new missing -- would lose the fact outright.
    #[tokio::test]
    async fn a_failed_supersede_leaves_the_new_memory_and_keeps_the_old_one_live() {
        struct SupersedeFails(MemStore);

        #[async_trait::async_trait]
        impl Store for SupersedeFails {
            async fn add(&self, new: NewMemory) -> Result<Memory> {
                self.0.add(new).await
            }
            async fn get(&self, id: Uuid) -> Result<Memory> {
                self.0.get(id).await
            }
            async fn update(&self, id: Uuid, u: MemoryUpdate) -> Result<Memory> {
                self.0.update(id, u).await
            }
            async fn delete(&self, id: Uuid) -> Result<()> {
                self.0.delete(id).await
            }
            async fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>> {
                self.0.search(q).await
            }
            async fn all(&self) -> Result<Vec<Memory>> {
                self.0.all().await
            }
            async fn vector_search(&self, q: VectorQuery) -> Result<Vec<SearchHit>> {
                self.0.vector_search(q).await
            }
            async fn missing_embeddings(&self, limit: usize) -> Result<Vec<Memory>> {
                self.0.missing_embeddings(limit).await
            }
            async fn set_embedding(&self, id: Uuid, e: &[f32]) -> Result<()> {
                self.0.set_embedding(id, e).await
            }
            async fn supersede(
                &self,
                _old: Uuid,
                _new: Option<Uuid>,
                _at: chrono::DateTime<chrono::Utc>,
            ) -> Result<()> {
                Err(Mem8Error::Store(
                    "simulated crash before invalidation".into(),
                ))
            }
        }

        let service = Memory8::new(Arc::new(SupersedeFails(MemStore::new())));
        let old = service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();

        let err = service
            .add(
                "storage is postgres",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::Store(_)));

        let all = service.all().await.unwrap();
        assert_eq!(all.len(), 2, "the new memory must survive the failure");
        assert!(
            all.iter().any(|m| m.content == "storage is postgres"),
            "the replacement was written before the invalidation was attempted"
        );

        let old = service.get(old.id).await.unwrap();
        assert!(
            old.invalid_at.is_none(),
            "the old memory must still be live, so the next write can repair the state"
        );
        assert_eq!(old.superseded_by, None);
    }

    /// Validation precedes the embed call, proven by counting model passes.
    ///
    /// A rejected supersede must cost nothing. Reading the line order shows the
    /// `return Err`s sit above `try_embed`, but a counter proves it holds at
    /// runtime for every rejection path.
    #[tokio::test]
    async fn a_rejected_supersede_costs_no_embedding_pass() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingEmbedder(Arc<AtomicUsize>);
        impl Embed for CountingEmbedder {
            fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
                self.0.fetch_add(texts.len(), Ordering::SeqCst);
                Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(MemStore::new());
        let service =
            Memory8::with_embedder(store.clone(), Arc::new(CountingEmbedder(calls.clone())));

        // One legitimate write, to show the embedder is genuinely wired up.
        let live = service
            .add(
                "storage is sqlite",
                Kind::Decision,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        let other = service
            .add(
                "other project fact",
                Kind::Decision,
                vec![],
                Some("p2".into()),
                None,
            )
            .await
            .unwrap();
        let baseline = calls.load(Ordering::SeqCst);
        assert_eq!(baseline, 2, "the embedder must actually be in use");

        // Rejection 1: the target does not exist.
        service
            .add(
                "a",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                Some(Uuid::new_v4()),
            )
            .await
            .unwrap_err();

        // Rejection 2: the target belongs to another project.
        service
            .add("b", Kind::Fact, vec![], Some("p1".into()), Some(other.id))
            .await
            .unwrap_err();

        // Rejection 3: the target is already dead.
        store.supersede(live.id, None, Utc::now()).await.unwrap();
        service
            .add("c", Kind::Fact, vec![], Some("p1".into()), Some(live.id))
            .await
            .unwrap_err();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            baseline,
            "no rejected supersede may spend a model pass"
        );
    }
}
