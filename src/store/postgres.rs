use super::Store;
use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;
use std::str::FromStr;
use uuid::Uuid;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id          UUID PRIMARY KEY,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    embedding   BYTEA
);
CREATE INDEX IF NOT EXISTS idx_project ON memories(project);
CREATE INDEX IF NOT EXISTS idx_project_kind ON memories(project, kind);
CREATE INDEX IF NOT EXISTS idx_content_fts
    ON memories USING GIN (to_tsvector('english', content));
"#;

pub struct PgStore {
    pool: PgPool,
}

fn store_err<E: std::fmt::Display>(e: E) -> Mem8Error {
    Mem8Error::Store(e.to_string())
}

impl PgStore {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(store_err)?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(store_err)?;
        Ok(Self { pool })
    }

    /// Truncate all memories. Test-support only.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn reset_for_tests(&self) -> Result<()> {
        sqlx::query("TRUNCATE memories")
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}

/// Builds a store error that names the offending column and value, so a
/// corrupt row surfaces as a diagnosable error instead of being silently
/// replaced with a fabricated default. Postgres's typed columns (uuid,
/// timestamptz, text[]) make most row corruption impossible at the driver
/// level; `kind` is the one column stored as free text, so it is the one
/// case that needs this.
fn column_parse_error(column: &'static str, bad_value: &str, source: impl std::fmt::Display) -> Mem8Error {
    Mem8Error::Store(format!("invalid {column} value {bad_value:?}: {source}"))
}

fn row_to_memory(row: &PgRow) -> Result<Memory> {
    let kind: String = row.get("kind");
    let parsed_kind = Kind::from_str(&kind).map_err(|e| column_parse_error("kind", &kind, e))?;
    Ok(Memory {
        id: row.get("id"),
        project: row.get("project"),
        kind: parsed_kind,
        content: row.get("content"),
        tags: row.get::<Vec<String>, _>("tags"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
        embedding: None,
    })
}

#[async_trait]
impl Store for PgStore {
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
        };

        sqlx::query(
            "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at, embedding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, NULL)",
        )
        .bind(memory.id)
        .bind(&memory.project)
        .bind(memory.kind.to_string())
        .bind(&memory.content)
        .bind(&memory.tags)
        .bind(memory.created_at)
        .bind(memory.updated_at)
        .execute(&self.pool)
        .await
        .map_err(store_err)?;

        Ok(memory)
    }

    async fn get(&self, id: Uuid) -> Result<Memory> {
        let row = sqlx::query("SELECT * FROM memories WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_err)?
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))?;
        row_to_memory(&row)
    }

    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory> {
        // Single statement: read-modify-write is expressed as an UPDATE ...
        // FROM ... RETURNING against the current row, so there is no
        // read-then-write gap for a concurrent caller to land in between.
        // Zero rows returned means the id did not exist -> NotFound.
        // COALESCE picks the new value when provided, else keeps the old one.
        let row = sqlx::query(
            "UPDATE memories SET
                content = COALESCE($1, content),
                kind = COALESCE($2, kind),
                tags = COALESCE($3, tags),
                updated_at = $4
             WHERE id = $5
             RETURNING *",
        )
        .bind(update.content)
        .bind(update.kind.map(|k| k.to_string()))
        .bind(update.tags)
        .bind(Utc::now())
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_err)?
        .ok_or_else(|| Mem8Error::NotFound(id.to_string()))?;

        row_to_memory(&row)
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let result = sqlx::query("DELETE FROM memories WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;

        if result.rows_affected() == 0 {
            return Err(Mem8Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>> {
        let rows = sqlx::query(
            "SELECT *, ts_rank(to_tsvector('english', content),
                               plainto_tsquery('english', $1)) AS score
             FROM memories
             WHERE to_tsvector('english', content) @@ plainto_tsquery('english', $1)
               AND ($2::bool OR project = $3)
               AND ($4::text IS NULL OR kind = $4)
               AND ($5::text[] = '{}' OR tags @> $5)
             ORDER BY score DESC
             LIMIT $6",
        )
        .bind(&query.text)
        .bind(query.global)
        .bind(query.project.clone().unwrap_or_default())
        .bind(query.kind.map(|k| k.to_string()))
        .bind(&query.tags)
        .bind(query.limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;

        rows.iter()
            .map(|row| {
                Ok(SearchHit {
                    memory: row_to_memory(row)?,
                    // Postgres's ts_rank is already higher-is-better, unlike
                    // SQLite's bm25() (lower-is-better, which SqliteStore
                    // negates). Do not negate this value.
                    score: row.get::<f32, _>("score") as f64,
                })
            })
            .collect()
    }

    async fn all(&self) -> Result<Vec<Memory>> {
        let rows = sqlx::query("SELECT * FROM memories ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(store_err)?;
        rows.iter().map(row_to_memory).collect()
    }
}
