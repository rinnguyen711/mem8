use super::Store;
use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery, VectorQuery};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;
use uuid::Uuid;

pub const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id          TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    embedding   BLOB
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

    fn init(conn: Connection) -> Result<Self> {
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
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(store_err)?;
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

    Ok(Memory {
        id: parsed_id,
        project: row.get("project")?,
        kind: parsed_kind,
        content: row.get("content")?,
        tags: parsed_tags,
        created_at,
        updated_at,
        embedding: None,
        superseded_by: None,
        invalid_at: None,
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
