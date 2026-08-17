use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery};
use crate::scope::detect_scope;
use crate::store::Store;
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
    let kept = if meaningful.is_empty() { &cleaned } else { &meaningful };

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
        "a", "an", "and", "are", "as", "at", "be", "but", "by", "did", "do", "does", "for",
        "from", "had", "has", "have", "how", "i", "in", "is", "it", "of", "on", "or", "our", "so",
        "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "to",
        "was", "we", "were", "what", "when", "where", "which", "who", "why", "will", "with",
        "would", "you", "your",
    ];
    let lower = term.to_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

/// The memory service. Owns validation and scope resolution so that the MCP
/// server and the CLI behave identically.
pub struct Memory8 {
    store: Arc<dyn Store>,
}

impl Memory8 {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    fn resolve_scope(&self, explicit: Option<String>) -> String {
        match explicit {
            Some(p) if !p.trim().is_empty() => p,
            _ => {
                let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                detect_scope(&cwd)
            }
        }
    }

    pub async fn add(
        &self,
        content: &str,
        kind: Kind,
        tags: Vec<String>,
        project: Option<String>,
    ) -> Result<Memory> {
        if content.trim().is_empty() {
            return Err(Mem8Error::InvalidInput("content must not be empty".into()));
        }

        let content = content.trim().to_string();
        let project = self.resolve_scope(project);

        // Revise a near-identical memory rather than storing it twice. Only the
        // resolved project is considered, so this can never merge across scopes.
        if let Some(existing) = self.find_duplicate(&content, &project).await {
            return self
                .store
                .update(
                    existing.id,
                    MemoryUpdate {
                        content: Some(content),
                        kind: Some(kind),
                        tags: Some(tags),
                    },
                )
                .await;
        }

        self.store
            .add(NewMemory { project, kind, content, tags })
            .await
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
            })
            .await
            .ok()?;

        hits.into_iter()
            .map(|hit| (word_overlap(content, &hit.memory.content), hit.memory))
            .filter(|(overlap, _)| *overlap >= DUPLICATE_THRESHOLD)
            .max_by(|(a, _), (b, _)| a.total_cmp(b))
            .map(|(_, memory)| memory)
    }

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
        self.store.update(id, MemoryUpdate { content, kind, tags }).await
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
        let project = if global { None } else { Some(self.resolve_scope(project)) };
        let scope = project.clone().unwrap_or_else(|| "*".to_string());

        let hits = self
            .store
            .search(SearchQuery { text: text.clone(), project, global, kind, tags, limit })
            .await?;

        if hits.is_empty() {
            log_missed_search(query, &text, &scope);
        }

        Ok(hits)
    }

    /// Every memory, for `mem8 export`.
    pub async fn all(&self) -> Result<Vec<Memory>> {
        self.store.all().await
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
        let err = svc.add("   ", Kind::Fact, vec![], Some("p1".into())).await.unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        assert!(err.to_string().contains("content"));
    }

    #[tokio::test]
    async fn add_uses_explicit_project_over_detection() {
        let svc = service();
        let m = svc.add("a fact", Kind::Fact, vec![], Some("explicit".into())).await.unwrap();
        assert_eq!(m.project, "explicit");
    }

    #[tokio::test]
    async fn add_falls_back_to_detected_scope() {
        let svc = service();
        let m = svc.add("a fact", Kind::Fact, vec![], None).await.unwrap();
        assert!(!m.project.is_empty(), "detected scope must never be empty");
    }

    #[tokio::test]
    async fn search_rejects_empty_query() {
        let svc = service();
        let err = svc.search("", None, false, None, vec![], None).await.unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
    }

    #[tokio::test]
    async fn search_clamps_limit_to_maximum() {
        let svc = service();
        for i in 0..60 {
            svc.add(&format!("fact number {i}"), Kind::Fact, vec![], Some("p1".into()))
                .await
                .unwrap();
        }
        let hits = svc
            .search("fact", Some("p1".into()), false, None, vec![], Some(999))
            .await
            .unwrap();
        assert!(hits.len() <= MAX_LIMIT, "limit must be clamped to {MAX_LIMIT}");
    }

    #[tokio::test]
    async fn search_defaults_limit_to_ten() {
        let svc = service();
        for i in 0..20 {
            svc.add(&format!("fact number {i}"), Kind::Fact, vec![], Some("p1".into()))
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
        assert!(cleaned.contains("backends") && cleaned.contains("stem"), "got: {cleaned}");
    }

    #[test]
    fn sanitize_keeps_function_words_when_they_are_all_there_is() {
        // Stripping every term would turn a legitimate search into an error.
        let cleaned = sanitize_fts_query("the who").unwrap();
        assert!(cleaned.contains("the") && cleaned.contains("who"), "got: {cleaned}");
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
            .add("We chose the porter tokenizer.", Kind::Decision, vec![], Some("p1".into()))
            .await
            .unwrap();
        let second = svc
            .add("We chose the porter tokenizer.", Kind::Decision, vec![], Some("p1".into()))
            .await
            .unwrap();

        assert_eq!(second.id, first.id, "a re-save should revise, not duplicate");
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
        )
        .await
        .unwrap();
        svc.add(
            "mem8 uses SQLite FTS5 with porter, matching how Postgres stems.",
            Kind::Decision,
            vec![],
            Some("p1".into()),
        )
        .await
        .unwrap();

        assert_eq!(svc.all().await.unwrap().len(), 2, "distinct wording must not merge");
    }

    #[tokio::test]
    async fn duplicate_detection_does_not_cross_projects() {
        let svc = service();
        svc.add("Run cargo fmt before committing.", Kind::Convention, vec![], Some("p1".into()))
            .await
            .unwrap();
        svc.add("Run cargo fmt before committing.", Kind::Convention, vec![], Some("p2".into()))
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
        svc.add("we chose rust", Kind::Decision, vec![], Some("p1".into())).await.unwrap();

        let hits = svc
            .search("kubernetes", Some("p1".into()), false, None, vec![], None)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn word_overlap_ignores_case_order_and_punctuation() {
        assert_eq!(word_overlap("Porter tokenizer, chosen.", "chosen porter TOKENIZER"), 1.0);
        assert_eq!(word_overlap("", "anything"), 0.0);
        assert!(word_overlap("we chose porter", "we picked snowball") < 0.5);
    }

    #[tokio::test]
    async fn update_trims_content_like_add_does() {
        let svc = service();
        let added = svc
            .add("  spaced out  ", Kind::Fact, vec![], Some("p1".into()))
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
}
