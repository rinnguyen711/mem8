pub mod postgres;
pub mod sqlite;

use crate::error::{Mem8Error, Result};
use crate::model::{Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery, VectorQuery};
use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Storage backend for memories.
///
/// Implementations must not leak backend-specific error types; wrap them in
/// `Mem8Error::Store`. This trait is the only place that knows about
/// persistence — `core` above it contains no SQL.
#[async_trait]
pub trait Store: Send + Sync {
    async fn add(&self, new: NewMemory) -> Result<Memory>;
    async fn get(&self, id: Uuid) -> Result<Memory>;
    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>>;
    /// Every memory, ordered by `created_at` ascending. Used by `mem8 export`.
    async fn all(&self) -> Result<Vec<Memory>>;

    /// Memories ranked by embedding similarity, nearest first.
    ///
    /// Filters apply before ranking, exactly as in `search`. Rows with no
    /// stored embedding are skipped — they are not "distant", they are
    /// unrepresented, and ranking them at all would put arbitrary memories in
    /// front of real matches.
    ///
    /// Backends that cannot do this return `Mem8Error::Unsupported` rather than
    /// an empty result: "this backend has no vector search" and "nothing
    /// matched" call for different responses from the caller, and an empty Vec
    /// cannot distinguish them.
    async fn vector_search(&self, query: VectorQuery) -> Result<Vec<SearchHit>>;

    /// Memories with no stored embedding, oldest first, for `mem8 reindex`.
    async fn missing_embeddings(&self, limit: usize) -> Result<Vec<Memory>>;

    /// Attach an embedding to an existing memory without touching its content
    /// or `updated_at`. Backfill is not an edit.
    async fn set_embedding(&self, id: Uuid, embedding: &[f32]) -> Result<()>;

    /// Mark `old` as no longer true as of `at`, replaced by `new`.
    ///
    /// `new` is `Option` because a memory can be known dead with an unknown
    /// successor: import may load a memory whose replacement is not present in
    /// the file, and the alternative — dropping the invalidation — would
    /// resurrect a fact the export recorded as dead. That is the failure
    /// round-tripping exists to prevent, so the store must be able to express
    /// it. `None` is reachable only from import; every other caller passes
    /// `Some`.
    ///
    /// The invariant is therefore: `invalid_at` is set whenever the memory is
    /// dead, and `superseded_by` is set whenever its successor is known. A row
    /// with `superseded_by` set but `invalid_at` NULL is incoherent and must
    /// never be written.
    ///
    /// Invalidation is **write-once**. A memory that already carries an
    /// `invalid_at` is rejected with `InvalidInput`; the existing timestamp is
    /// never moved. Moving it later would resurrect the memory for every
    /// `as_of` query between the old and new instants — a fact that was dead at
    /// T would start reading as live at T — and that corrupts an append-only
    /// temporal record rather than merely losing an update. Only the store can
    /// guarantee this, so it is enforced here and not left to `core`.
    ///
    /// Returns `NotFound` if `old` does not exist. Validation of the
    /// *relationship* — same project, correct successor — belongs to `core`,
    /// not here: the store records the fact, it does not police intent. The
    /// write-once rule is the exception, because it protects the integrity of
    /// the record itself rather than the caller's intent.
    ///
    /// `at` is truncated to microseconds by every implementation — see
    /// [`truncate_for_storage`]. A caller therefore cannot rely on
    /// nanosecond precision surviving the write.
    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()>;
}

/// Round an invalidation instant down to microsecond precision.
///
/// Postgres's `TIMESTAMPTZ` holds only microseconds, while SQLite stores the
/// same field as RFC3339 text and keeps all nine digits. An untruncated instant
/// therefore lands differently in the two backends, and an `as_of` falling
/// between the truncated and full values reads the memory as dead on Postgres
/// and live on SQLite — the backends disagreeing about a single instant.
///
/// The divergence is invisible on macOS, where `Utc::now()` is already
/// microsecond-resolution, and live on Linux, where it returns true
/// nanoseconds. That makes it exactly the kind of bug that passes locally and
/// fails in CI, so it is fixed at the point of entry rather than documented:
/// every `supersede` implementation truncates before storing, so all three
/// agree regardless of which one a caller holds.
///
/// Truncating (rather than rounding to nearest) keeps the stored instant at or
/// before the one the caller supplied, so an invalidation never moves later
/// than the event it records.
pub fn truncate_for_storage(at: DateTime<Utc>) -> DateTime<Utc> {
    at.trunc_subsecs(6)
}

/// In-memory `Store` used by `core` unit tests. Substring matching stands in
/// for full-text search; ranking fidelity is covered by the contract suite
/// against the real backends.
///
/// Test support only — do not wire this in as a production backend. Its
/// `Mutex` guards are unwrapped without poisoning recovery, so a panic taken
/// while the lock is held would poison it and fail every later call on the
/// same instance. The real backends carry the availability requirement that
/// a store failure must not end the session.
pub struct MemStore {
    rows: Mutex<Vec<Memory>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self {
            rows: Mutex::new(Vec::new()),
        }
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a memory is visible under a query's temporal filters.
///
/// Three modes, one predicate:
/// - default: live only (`invalid_at IS NULL`)
/// - `include_superseded`: everything
/// - `as_of: T`: what was believed at T
///
/// `as_of` wins when both are set: it already specifies exactly which rows
/// count. The tool boundary rejects that combination outright, but the store
/// stays total over its input so a direct caller cannot produce nonsense.
fn visible_at(memory: &Memory, include_superseded: bool, as_of: Option<DateTime<Utc>>) -> bool {
    match as_of {
        Some(t) => memory.created_at <= t && memory.invalid_at.is_none_or(|invalid| invalid > t),
        None => include_superseded || memory.invalid_at.is_none(),
    }
}

#[async_trait]
impl Store for MemStore {
    async fn add(&self, new: NewMemory) -> Result<Memory> {
        let now = Utc::now();
        let memory = Memory {
            id: Uuid::new_v4(),
            project: new.project,
            kind: new.kind,
            content: new.content,
            tags: new.tags,
            created_at: now,
            updated_at: now,
            embedding: new.embedding,
            superseded_by: None,
            invalid_at: None,
        };
        self.rows.lock().unwrap().push(memory.clone());
        Ok(memory)
    }

    async fn get(&self, id: Uuid) -> Result<Memory> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))
    }

    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))?;

        if let Some(content) = update.content {
            row.content = content;
        }
        if let Some(kind) = update.kind {
            row.kind = kind;
        }
        if let Some(tags) = update.tags {
            row.tags = tags;
        }
        if let Some(embedding) = update.embedding {
            row.embedding = Some(embedding);
        }
        row.updated_at = Utc::now();
        Ok(row.clone())
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|m| m.id != id);
        if rows.len() == before {
            return Err(Mem8Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>> {
        // `query.text` arrives pre-sanitized by `core::sanitize_fts_query` as
        // space-separated double-quoted phrases (e.g. `"auth-token" "login"`).
        // A real FTS engine ANDs separate terms together, so this stand-in
        // strips the quoting and requires every term to appear as a substring,
        // rather than treating the whole sanitized string as one literal
        // needle (which would never match once terms carry quotes).
        let needles: Vec<String> = query
            .text
            .split(' ')
            .map(|t| t.trim_matches('"').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let rows = self.rows.lock().unwrap();
        let hits = rows
            .iter()
            .filter(|m| query.global || query.project.as_deref() == Some(m.project.as_str()))
            .filter(|m| query.kind.is_none_or(|k| k == m.kind))
            .filter(|m| query.tags.iter().all(|t| m.tags.contains(t)))
            .filter(|m| {
                let content = m.content.to_lowercase();
                needles.iter().all(|n| content.contains(n.as_str()))
            })
            .filter(|m| visible_at(m, query.include_superseded, query.as_of))
            .take(query.limit)
            .map(|m| SearchHit {
                memory: m.clone(),
                score: 1.0,
            })
            .collect();
        Ok(hits)
    }

    async fn all(&self) -> Result<Vec<Memory>> {
        let mut rows = self.rows.lock().unwrap().clone();
        rows.sort_by_key(|m| m.created_at);
        Ok(rows)
    }

    async fn vector_search(&self, query: VectorQuery) -> Result<Vec<SearchHit>> {
        let rows = self.rows.lock().unwrap();
        let mut hits: Vec<SearchHit> = rows
            .iter()
            .filter(|m| query.global || query.project.as_deref() == Some(m.project.as_str()))
            .filter(|m| query.kind.is_none_or(|k| k == m.kind))
            .filter(|m| query.tags.iter().all(|t| m.tags.contains(t)))
            .filter(|m| visible_at(m, query.include_superseded, query.as_of))
            .filter_map(|m| {
                // No embedding means unrepresented, not distant: skip rather
                // than score, matching what the real backends do with NULL.
                let stored = m.embedding.as_ref()?;
                Some(SearchHit {
                    memory: m.clone(),
                    score: crate::embed::cosine_similarity(&query.embedding, stored) as f64,
                })
            })
            .collect();

        hits.sort_by(|a, b| b.score.total_cmp(&a.score));
        hits.truncate(query.limit);
        Ok(hits)
    }

    async fn missing_embeddings(&self, limit: usize) -> Result<Vec<Memory>> {
        let mut rows: Vec<Memory> = self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.embedding.is_none())
            .cloned()
            .collect();
        rows.sort_by_key(|m| m.created_at);
        rows.truncate(limit);
        Ok(rows)
    }

    async fn set_embedding(&self, id: Uuid, embedding: &[f32]) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))?;
        row.embedding = Some(embedding.to_vec());
        Ok(())
    }

    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()> {
        // Truncated here as well, so `core` tests against `MemStore` observe
        // the same precision they will get from a real backend.
        let at = truncate_for_storage(at);
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|m| m.id == old)
            .ok_or_else(|| Mem8Error::NotFound(old.to_string()))?;

        // Write-once, matching the real backends. `MemStore` exists so that
        // `core` tests exercise the same contract they will meet in
        // production; letting it accept a second invalidation here would hide
        // the SQL backends' rejection until integration time.
        if let Some(existing) = row.invalid_at {
            return Err(Mem8Error::InvalidInput(format!(
                "memory {old} is already superseded as of {}",
                existing.to_rfc3339()
            )));
        }

        row.superseded_by = new;
        row.invalid_at = Some(at);
        Ok(())
    }
}

/// Default SQLite location when `MEM8_DB` is unset.
pub fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mem8")
        .join("mem8.db")
}

/// Open the backend named by a connection URL.
pub async fn open_from_url(url: &str) -> Result<Arc<dyn Store>> {
    if let Some(path) = url.strip_prefix("sqlite://") {
        let store = sqlite::SqliteStore::open(std::path::Path::new(path))?;
        return Ok(Arc::new(store));
    }
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        let store = postgres::PgStore::connect(url).await?;
        return Ok(Arc::new(store));
    }
    Err(Mem8Error::InvalidInput(format!(
        "unsupported MEM8_DB value '{url}'; expected a sqlite:// or postgres:// URL"
    )))
}

/// Open the backend selected by `MEM8_DB`, defaulting to SQLite under the home
/// directory.
pub async fn open_from_env() -> Result<Arc<dyn Store>> {
    match std::env::var("MEM8_DB") {
        Ok(url) if !url.trim().is_empty() => open_from_url(&url).await,
        _ => {
            let store = sqlite::SqliteStore::open(&default_db_path())?;
            Ok(Arc::new(store))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, NewMemory, SearchQuery};

    fn new_memory(project: &str, content: &str) -> NewMemory {
        NewMemory {
            project: project.into(),
            kind: Kind::Decision,
            content: content.into(),
            tags: vec![],
            ..Default::default()
        }
    }

    fn query(text: &str, project: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            project: Some(project.into()),
            global: false,
            kind: None,
            tags: vec![],
            limit: 10,
            include_superseded: false,
            as_of: None,
        }
    }

    #[tokio::test]
    async fn add_then_get_returns_same_content() {
        let store = MemStore::new();
        let added = store.add(new_memory("p1", "use rust")).await.unwrap();
        let fetched = store.get(added.id).await.unwrap();
        assert_eq!(fetched.content, "use rust");
    }

    #[tokio::test]
    async fn get_missing_id_is_not_found() {
        let store = MemStore::new();
        let err = store.get(uuid::Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, Mem8Error::NotFound(_)));
    }

    #[tokio::test]
    async fn search_filters_by_project() {
        let store = MemStore::new();
        store.add(new_memory("p1", "use rust")).await.unwrap();
        store.add(new_memory("p2", "use rust")).await.unwrap();

        let hits = store.search(query("rust", "p1")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory.project, "p1");
    }

    #[tokio::test]
    async fn delete_removes_the_memory() {
        let store = MemStore::new();
        let added = store.add(new_memory("p1", "use rust")).await.unwrap();
        store.delete(added.id).await.unwrap();
        assert!(store.get(added.id).await.is_err());
    }

    #[test]
    fn default_path_is_under_home_dot_mem8() {
        let path = default_db_path();
        assert!(path.ends_with("mem8.db"));
        assert!(path.to_string_lossy().contains(".mem8"));
    }

    #[tokio::test]
    async fn sqlite_url_opens_a_file_backend() {
        let dir = std::env::temp_dir().join(format!("mem8-sel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let url = format!("sqlite://{}", path.display());

        let store = open_from_url(&url).await.unwrap();
        store
            .add(crate::model::NewMemory {
                project: "p".into(),
                kind: crate::model::Kind::Fact,
                content: "persisted".into(),
                tags: vec![],
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(path.exists(), "sqlite:// URL must create the database file");

        // Release the connection before removing the directory; Windows keeps
        // the file locked while the handle is open.
        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_hides_superseded_by_default() {
        let store = MemStore::new();
        let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
        let new = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
        store
            .supersede(old.id, Some(new.id), Utc::now())
            .await
            .unwrap();

        let hits = store.search(query("sqlite", "p1")).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.memory.id).collect();
        assert!(!ids.contains(&old.id), "superseded memory must be hidden");
        assert!(ids.contains(&new.id), "its replacement must be returned");
    }

    #[tokio::test]
    async fn include_superseded_returns_both() {
        let store = MemStore::new();
        let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
        let new = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
        store
            .supersede(old.id, Some(new.id), Utc::now())
            .await
            .unwrap();

        let q = SearchQuery {
            include_superseded: true,
            ..query("sqlite", "p1")
        };
        assert_eq!(store.search(q).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_still_returns_a_superseded_memory() {
        let store = MemStore::new();
        let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
        let new = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
        store
            .supersede(old.id, Some(new.id), Utc::now())
            .await
            .unwrap();

        // Hidden from discovery, not deleted. This distinction is the whole reason
        // supersession is not delete_memory.
        let got = store.get(old.id).await.unwrap();
        assert_eq!(got.content, "use sqlite");
        assert_eq!(got.superseded_by, Some(new.id));
    }

    #[tokio::test]
    async fn superseding_twice_is_rejected_and_keeps_the_first_timestamp() {
        let store = MemStore::new();
        let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
        let first = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
        let second = store.add(new_memory("p1", "use postgres")).await.unwrap();

        // Truncated to what the store will actually keep: `supersede` narrows
        // the instant to microseconds so the two backends agree, and
        // `Utc::now()` carries finer resolution on Linux and Windows than on
        // macOS. Comparing against the raw instant would pass only on a clock
        // whose sub-microsecond digits are already zero.
        let at = truncate_for_storage(Utc::now());
        store.supersede(old.id, Some(first.id), at).await.unwrap();

        // A later invalidation must not move the earlier one: an `as_of`
        // between the two instants would otherwise see a memory that was
        // already dead.
        let err = store
            .supersede(old.id, Some(second.id), at + chrono::Duration::hours(1))
            .await
            .unwrap_err();
        assert!(
            matches!(err, Mem8Error::InvalidInput(_)),
            "a second supersede must be InvalidInput, got: {err:?}"
        );

        let got = store.get(old.id).await.unwrap();
        assert_eq!(got.invalid_at, Some(at), "the original invalid_at must stand");
        assert_eq!(
            got.superseded_by,
            Some(first.id),
            "the original successor must stand"
        );
    }

    /// A stored invalidation is comparable against the truncated instant, not
    /// the raw one, on every platform.
    ///
    /// The instant here is constructed with non-zero sub-microsecond digits
    /// rather than sampled from the clock, because `Utc::now()` on macOS
    /// already returns microsecond resolution — so a sampled instant makes this
    /// vacuous there while Linux and Windows fail. That asymmetry is exactly
    /// what broke CI once: a sibling test compared against the raw instant and
    /// passed only on the developer's machine.
    #[tokio::test]
    async fn a_stored_invalidation_is_truncated_to_microseconds() {
        use chrono::TimeZone;
        let store = MemStore::new();
        let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
        let first = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
        let second = store.add(new_memory("p1", "use postgres")).await.unwrap();

        // Non-zero sub-microsecond digits, exactly what CI's clock produces.
        let raw = Utc.timestamp_nanos(1_787_197_906_743_519_789);
        assert_ne!(truncate_for_storage(raw), raw, "probe must use a ragged instant");

        let at = truncate_for_storage(raw);
        store.supersede(old.id, Some(first.id), at).await.unwrap();
        let err = store
            .supersede(old.id, Some(second.id), at + chrono::Duration::hours(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Mem8Error::InvalidInput(_)));
        let got = store.get(old.id).await.unwrap();
        assert_eq!(got.invalid_at, Some(at), "the original invalid_at must stand");
    }

    #[tokio::test]
    async fn unknown_url_scheme_is_an_error() {
        let result = open_from_url("mysql://localhost/db").await;
        let message = match result {
            Ok(_) => panic!("expected an error for an unsupported URL scheme"),
            Err(e) => e.to_string(),
        };
        assert!(
            message.contains("mysql"),
            "error should name the offending scheme, got: {message}"
        );
    }
}
