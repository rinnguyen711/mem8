use super::{truncate_for_storage, Store};
use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery, VectorQuery};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use uuid::Uuid;

pub const SCHEMA_VERSION: i32 = 2;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id          TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    embedding   BLOB,
    superseded_by TEXT,
    invalid_at    TEXT
);
CREATE INDEX IF NOT EXISTS idx_project ON memories(project);
CREATE INDEX IF NOT EXISTS idx_project_kind ON memories(project, kind);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    content,
    content='memories',
    content_rowid='rowid',
    tokenize='porter'
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, content)
        VALUES ('delete', old.rowid, old.content);
    INSERT INTO memories_fts(rowid, content) VALUES (new.rowid, new.content);
END;
"#;

/// Version 2: record that one fact replaced another.
///
/// Both columns are nullable and existing rows migrate to NULL/NULL, so every
/// memory that exists today stays live and keeps being returned.
///
/// `invalid_at` is TEXT holding RFC3339, and the search predicates in Tasks 5-6
/// compare it as text rather than parsing it. That is only correct while every
/// writer uses plain `to_rfc3339()`, which renders UTC as `+00:00`. The `Z`
/// form must NOT be used for these columns -- `to_rfc3339_opts(.., true)`
/// included -- because `'Z'` (0x5A) sorts after `'+'` (0x2B), so the very same
/// instant would compare as later than its `+00:00` spelling and an `as_of`
/// boundary would silently flip. No existing test would catch it.
const MIGRATE_V2: &[&str] = &[
    "ALTER TABLE memories ADD COLUMN superseded_by TEXT",
    "ALTER TABLE memories ADD COLUMN invalid_at TEXT",
];

pub struct SqliteStore {
    conn: Mutex<Connection>,
}

fn store_err<E: std::fmt::Display>(e: E) -> Mem8Error {
    Mem8Error::Store(e.to_string())
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(store_err)?;
        }
        let conn = Connection::open(path).map_err(store_err)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(store_err)?;
        Self::init(conn)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA).map_err(store_err)?;

        let found: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(store_err)?;

        if found > SCHEMA_VERSION {
            return Err(Mem8Error::Migration {
                found,
                expected: SCHEMA_VERSION,
            });
        }

        if found < SCHEMA_VERSION {
            // The ALTERs and the version bump share one transaction, so an
            // interrupted upgrade leaves a v1 database rather than a
            // half-migrated one. `CREATE TABLE IF NOT EXISTS` above is a no-op
            // on an existing table, which is exactly why the columns cannot be
            // added there and need a real migration step.
            let tx = conn.transaction().map_err(store_err)?;
            // `found == 0` is a brand-new database and `found == 1` a shipped
            // v1 one; both need the v2 columns, which is why the guard is
            // `< 2` rather than `== 1`. (`postgres.rs`'s `migrate` documents
            // the same split via its `unwrap_or(1)`.)
            if found < 2 {
                for statement in MIGRATE_V2 {
                    // Swallowing duplicate-column is load-bearing twice over,
                    // so do not "simplify" it away:
                    //
                    // 1. A fresh database already got both columns from
                    //    SCHEMA, so the ALTERs are *expected* to fail there.
                    // 2. Two processes can open the same file at once. The
                    //    loser sees the columns the winner already added, and
                    //    treating that as success is what makes the race
                    //    benign instead of failing one process's startup.
                    //
                    // Matched on the message because there is no typed
                    // alternative: `duplicate column name` and `no such table`
                    // are both bare `SQLITE_ERROR` with identical primary and
                    // extended codes, so an extended-code match cannot tell
                    // them apart and would swallow every ALTER failure
                    // including a genuinely missing table. `starts_with`
                    // rather than `contains`, so the match cannot be satisfied
                    // by an unrelated error that merely quotes the phrase.
                    match tx.execute(statement, []) {
                        Ok(_) => {}
                        Err(e) if e.to_string().starts_with("duplicate column name") => {}
                        Err(e) => return Err(store_err(e)),
                    }
                }
            }
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(store_err)?;
            tx.commit().map_err(store_err)?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

/// Builds a `rusqlite::Error` that names the offending column and value, so a
/// corrupt row surfaces as a diagnosable store error instead of being
/// silently replaced with a fabricated default.
fn column_parse_error(
    column: &'static str,
    bad_value: &str,
    source: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("invalid {column} value {bad_value:?}: {source}").into(),
    )
}

fn row_to_memory(row: &Row) -> rusqlite::Result<Memory> {
    let id: String = row.get("id")?;
    let kind: String = row.get("kind")?;
    let tags: String = row.get("tags")?;
    let created: String = row.get("created_at")?;
    let updated: String = row.get("updated_at")?;

    let parsed_id = Uuid::parse_str(&id).map_err(|e| column_parse_error("id", &id, e))?;
    let parsed_kind = Kind::from_str(&kind).map_err(|e| column_parse_error("kind", &kind, e))?;
    let parsed_tags: Vec<String> =
        serde_json::from_str(&tags).map_err(|e| column_parse_error("tags", &tags, e))?;
    let created_at = DateTime::parse_from_rfc3339(&created)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| column_parse_error("created_at", &created, e))?;
    let updated_at = DateTime::parse_from_rfc3339(&updated)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|e| column_parse_error("updated_at", &updated, e))?;

    let superseded_by = row
        .get::<_, Option<String>>("superseded_by")?
        .map(|s| Uuid::parse_str(&s).map_err(|e| column_parse_error("superseded_by", &s, e)))
        .transpose()?;
    let invalid_at = row
        .get::<_, Option<String>>("invalid_at")?
        .map(|s| {
            DateTime::parse_from_rfc3339(&s)
                .map(|d| d.with_timezone(&Utc))
                .map_err(|e| column_parse_error("invalid_at", &s, e))
        })
        .transpose()?;

    Ok(Memory {
        id: parsed_id,
        project: row.get("project")?,
        kind: parsed_kind,
        content: row.get("content")?,
        tags: parsed_tags,
        created_at,
        updated_at,
        embedding: None,
        superseded_by,
        invalid_at,
    })
}

#[async_trait]
impl Store for SqliteStore {
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
            embedding: None,
            superseded_by: None,
            invalid_at: None,
        };

        let tags = serde_json::to_string(&memory.tags).map_err(store_err)?;
        self.conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at, embedding)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                params![
                    memory.id.to_string(),
                    memory.project,
                    memory.kind.to_string(),
                    memory.content,
                    tags,
                    memory.created_at.to_rfc3339(),
                    memory.updated_at.to_rfc3339(),
                ],
            )
            .map_err(store_err)?;

        Ok(memory)
    }

    async fn get(&self, id: Uuid) -> Result<Memory> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT * FROM memories WHERE id = ?1",
            params![id.to_string()],
            row_to_memory,
        )
        .optional()
        .map_err(store_err)?
        .ok_or_else(|| Mem8Error::NotFound(id.to_string()))
    }

    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory> {
        let current = self.get(id).await?;
        let content = update.content.unwrap_or(current.content);
        let kind = update.kind.unwrap_or(current.kind);
        let tags = update.tags.unwrap_or(current.tags);
        let updated_at = Utc::now();
        let tags_json = serde_json::to_string(&tags).map_err(store_err)?;

        self.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE memories SET content = ?1, kind = ?2, tags = ?3, updated_at = ?4
                 WHERE id = ?5",
                params![
                    content,
                    kind.to_string(),
                    tags_json,
                    updated_at.to_rfc3339(),
                    id.to_string()
                ],
            )
            .map_err(store_err)?;

        Ok(Memory {
            content,
            kind,
            tags,
            updated_at,
            ..current
        })
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let changed = self
            .conn
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM memories WHERE id = ?1",
                params![id.to_string()],
            )
            .map_err(store_err)?;

        if changed == 0 {
            return Err(Mem8Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>> {
        let mut sql = String::from(
            "SELECT m.*, bm25(memories_fts) AS score
             FROM memories_fts
             JOIN memories m ON m.rowid = memories_fts.rowid
             WHERE memories_fts MATCH ?1",
        );
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.text.clone())];

        if !query.global {
            if let Some(project) = &query.project {
                binds.push(Box::new(project.clone()));
                sql.push_str(&format!(" AND m.project = ?{}", binds.len()));
            }
        }
        if let Some(kind) = query.kind {
            binds.push(Box::new(kind.to_string()));
            sql.push_str(&format!(" AND m.kind = ?{}", binds.len()));
        }

        // Three modes, one predicate. `as_of` already specifies exactly which
        // rows count, so it takes precedence over `include_superseded`; the
        // tool boundary rejects a caller that sets both, but the store stays
        // total over its input.
        match query.as_of {
            Some(t) => {
                // Bound once: both comparisons must use the identical string,
                // which is the property the text-ordering argument rests on.
                let t_str = t.to_rfc3339();
                binds.push(Box::new(t_str.clone()));
                sql.push_str(&format!(" AND m.created_at <= ?{}", binds.len()));
                binds.push(Box::new(t_str));
                sql.push_str(&format!(
                    " AND (m.invalid_at IS NULL OR m.invalid_at > ?{})",
                    binds.len()
                ));
            }
            None if !query.include_superseded => {
                sql.push_str(" AND m.invalid_at IS NULL");
            }
            None => {}
        }

        sql.push_str(" ORDER BY score");

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(store_err)?;
        let params: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let score: f64 = row.get("score")?;
                Ok(SearchHit {
                    memory: row_to_memory(row)?,
                    score: -score,
                })
            })
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;

        // Tag filtering happens here rather than in SQL: tags are stored as a
        // JSON string in SQLite, so an AND-across-tags predicate is clearer in
        // Rust than in nested json_each subqueries. LIMIT is intentionally not
        // in the SQL above — it must apply AFTER this filter, matching
        // MemStore's semantics: apply every filter, then truncate to `limit`.
        Ok(rows
            .into_iter()
            .filter(|hit| query.tags.iter().all(|t| hit.memory.tags.contains(t)))
            .take(query.limit)
            .collect())
    }

    async fn all(&self) -> Result<Vec<Memory>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT * FROM memories ORDER BY created_at ASC")
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], row_to_memory)
            .map_err(store_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_err)?;
        Ok(rows)
    }

    /// Not supported: SQLite is keyword-only.
    ///
    /// Semantic search was scoped to Postgres deliberately. Returning an error
    /// rather than an empty result is the point — an empty Vec would be
    /// indistinguishable from "nothing matched", and a caller would silently
    /// believe it had searched semantically when it had not.
    ///
    /// If this ever gains a real implementation, it must apply the identical
    /// `as_of` / `include_superseded` predicate that `search` applies above,
    /// so the two search paths agree on what counts as live.
    async fn vector_search(&self, _query: VectorQuery) -> Result<Vec<SearchHit>> {
        Err(Mem8Error::Unsupported {
            feature: "semantic search".into(),
            backend: "the SQLite backend".into(),
        })
    }

    /// Always empty: nothing here can hold an embedding, so nothing is missing
    /// one. `mem8 reindex` therefore finds no work rather than failing.
    async fn missing_embeddings(&self, _limit: usize) -> Result<Vec<Memory>> {
        Ok(Vec::new())
    }

    async fn set_embedding(&self, _id: Uuid, _embedding: &[f32]) -> Result<()> {
        Err(Mem8Error::Unsupported {
            feature: "storing embeddings".into(),
            backend: "the SQLite backend".into(),
        })
    }

    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()> {
        // One statement sets both columns, so `invalid_at` is never set without
        // the successor being decided in the same write. `new` may be NULL --
        // known dead, successor unknown -- but the reverse (a successor with no
        // invalidation time) is incoherent and unreachable from here.
        //
        // `AND invalid_at IS NULL` makes invalidation write-once. Without it a
        // second call moves `invalid_at` forward, and every `as_of` query
        // between the old and new instants starts seeing a memory that was
        // already dead at that point -- an append-only temporal record silently
        // rewritten. The guard lives in the WHERE clause rather than in a
        // read-then-write, so two concurrent callers cannot both pass a check
        // and then both write.
        //
        // `at` is written with plain `to_rfc3339()` (the `+00:00` form) because
        // the search predicates compare this column as text. See MIGRATE_V2.
        //
        // Truncated to microseconds first. SQLite would happily keep all nine
        // digits, but Postgres's TIMESTAMPTZ cannot, and a store-dependent
        // precision makes the two disagree about an `as_of` between the
        // truncated and full values. See `truncate_for_storage`.
        //
        // This UPDATE fires the unconditional `memories_au` FTS trigger, which
        // deletes and re-inserts the row's FTS entry. Harmless -- `content` is
        // unchanged, so the entry is rewritten identically -- and narrowing the
        // trigger would mean changing shipped schema DDL for no behavioural
        // gain.
        let at = truncate_for_storage(at);
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "UPDATE memories SET superseded_by = ?1, invalid_at = ?2
                 WHERE id = ?3 AND invalid_at IS NULL",
                params![new.map(|n| n.to_string()), at.to_rfc3339(), old.to_string()],
            )
            .map_err(store_err)?;

        if changed == 0 {
            // Zero rows now means either "no such memory" or "already
            // superseded", and the caller needs to tell those apart: one is a
            // bad id, the other a rejected duplicate. Re-read to decide.
            let existing: Option<Option<String>> = conn
                .query_row(
                    "SELECT invalid_at FROM memories WHERE id = ?1",
                    params![old.to_string()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(store_err)?;

            return match existing {
                None => Err(Mem8Error::NotFound(old.to_string())),
                Some(invalid_at) => Err(Mem8Error::InvalidInput(format!(
                    "memory {old} is already superseded as of {}",
                    invalid_at.unwrap_or_else(|| "an unknown time".into())
                ))),
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, NewMemory, SearchQuery};

    fn store() -> SqliteStore {
        SqliteStore::open_in_memory().unwrap()
    }

    fn new_memory(project: &str, content: &str) -> NewMemory {
        NewMemory {
            project: project.into(),
            kind: Kind::Decision,
            content: content.into(),
            tags: vec!["rust".into()],
            ..Default::default()
        }
    }

    fn new_memory_with_tags(project: &str, content: &str, tags: Vec<&str>) -> NewMemory {
        NewMemory {
            project: project.into(),
            kind: Kind::Decision,
            content: content.into(),
            tags: tags.into_iter().map(String::from).collect(),
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
    async fn add_then_get_roundtrips_all_fields() {
        let s = store();
        let added = s
            .add(new_memory("p1", "we chose rust for the binary"))
            .await
            .unwrap();
        let got = s.get(added.id).await.unwrap();
        assert_eq!(got.content, "we chose rust for the binary");
        assert_eq!(got.kind, Kind::Decision);
        assert_eq!(got.tags, vec!["rust".to_string()]);
        assert_eq!(got.project, "p1");
        assert!(got.embedding.is_none());
    }

    #[tokio::test]
    async fn fts_finds_memory_by_word() {
        let s = store();
        s.add(new_memory("p1", "we chose rust for the binary"))
            .await
            .unwrap();
        let hits = s.search(query("rust", "p1")).await.unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn fts_index_updates_after_content_change() {
        let s = store();
        let added = s.add(new_memory("p1", "we chose rust")).await.unwrap();
        s.update(
            added.id,
            crate::model::MemoryUpdate {
                content: Some("we chose python".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(s.search(query("rust", "p1")).await.unwrap().is_empty());
        assert_eq!(s.search(query("python", "p1")).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fts_index_drops_deleted_rows() {
        let s = store();
        let added = s.add(new_memory("p1", "we chose rust")).await.unwrap();
        s.delete(added.id).await.unwrap();
        assert!(s.search(query("rust", "p1")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn tag_filter_applies_before_limit() {
        let s = store();

        // 5 memories all match the FTS word "widget". Only 3 carry "keep";
        // the other 2 carry "drop". A naive impl applies SQL LIMIT (3) first,
        // then filters by tag in Rust, which can strip tagged rows that never
        // made it past the SQL limit and under-return. The correct semantic
        // (matching MemStore) is: filter by tag first, then limit to 3.
        s.add(new_memory_with_tags("p1", "widget one", vec!["drop"]))
            .await
            .unwrap();
        s.add(new_memory_with_tags("p1", "widget two", vec!["drop"]))
            .await
            .unwrap();
        s.add(new_memory_with_tags("p1", "widget three", vec!["keep"]))
            .await
            .unwrap();
        s.add(new_memory_with_tags("p1", "widget four", vec!["keep"]))
            .await
            .unwrap();
        s.add(new_memory_with_tags("p1", "widget five", vec!["keep"]))
            .await
            .unwrap();

        let mut q = query("widget", "p1");
        q.tags = vec!["keep".into()];
        q.limit = 3;

        let hits = s.search(q).await.unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits
            .iter()
            .all(|h| h.memory.tags.contains(&"keep".to_string())));
    }

    #[tokio::test]
    async fn corrupt_row_errors_instead_of_fabricating() {
        let s = store();
        let added = s
            .add(new_memory("p1", "we chose rust for the binary"))
            .await
            .unwrap();

        s.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE memories SET kind = 'banana' WHERE id = ?1",
                params![added.id.to_string()],
            )
            .unwrap();

        let result = s.get(added.id).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("banana"),
            "error message should mention the bad value: {msg}"
        );
    }
}
