use super::Store;
use crate::error::{Mem8Error, Result};
use crate::model::{Kind, Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery, VectorQuery};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;
use std::str::FromStr;
use uuid::Uuid;

/// Schema version this binary understands.
///
/// 1 — the original table, with `embedding BYTEA` as an unused placeholder.
/// 2 — `embedding` becomes a real `vector(384)` column with an HNSW index.
pub const PG_SCHEMA_VERSION: i32 = 2;

/// The base table, as it has existed since v1. `migrate` brings it forward from
/// here.
///
/// Issued statement by statement rather than as one `raw_sql` batch, because a
/// `raw_sql` future is not provably `Send` and would make `connect` unusable
/// from a spawned task.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS memories (
        id          UUID PRIMARY KEY,
        project     TEXT NOT NULL,
        kind        TEXT NOT NULL,
        content     TEXT NOT NULL,
        tags        TEXT[] NOT NULL DEFAULT '{}',
        created_at  TIMESTAMPTZ NOT NULL,
        updated_at  TIMESTAMPTZ NOT NULL,
        embedding   BYTEA
    )",
    "CREATE INDEX IF NOT EXISTS idx_project ON memories(project)",
    "CREATE INDEX IF NOT EXISTS idx_project_kind ON memories(project, kind)",
    "CREATE INDEX IF NOT EXISTS idx_content_fts
        ON memories USING GIN (to_tsvector('english', content))",
];

/// Version 2: turn the unused `embedding BYTEA` placeholder into a real
/// pgvector column.
///
/// Dropping the old column loses nothing — no code path ever wrote it, so it is
/// NULL in every row. `migrate` verifies that against the live table rather than
/// trusting this comment.
const MIGRATE_V2: &[&str] = &[
    "ALTER TABLE memories DROP COLUMN IF EXISTS embedding",
    "ALTER TABLE memories ADD COLUMN embedding vector(384)",
    "CREATE INDEX IF NOT EXISTS idx_embedding
        ON memories USING hnsw (embedding vector_cosine_ops)",
];

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
        // The base schema is created inside `migrate`, under the same advisory
        // lock as the migration itself. `CREATE TABLE IF NOT EXISTS` races just
        // as `CREATE EXTENSION` does when two processes start together.
        migrate(&pool).await?;
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

    /// The pool, so migration tests can manipulate schema state directly.
    #[cfg(any(test, feature = "test-support"))]
    pub fn pool_for_tests(&self) -> &PgPool {
        &self.pool
    }
}

/// Bring the schema up to `PG_SCHEMA_VERSION`, refusing a database that is
/// newer than this binary understands.
///
/// `SqliteStore` has done this since v1 via `PRAGMA user_version`; Postgres had
/// no equivalent, so an old binary pointed at a new database would misread it
/// silently. Postgres has no built-in user version, so a one-row `mem8_meta`
/// table stands in for the pragma.
///
/// The whole check runs in one transaction behind an advisory lock, because two
/// mem8 processes can start at once. Without it both observe "not yet
/// migrated" and both run the DDL, and the loser fails.
async fn migrate(pool: &PgPool) -> Result<()> {
    let mut tx = pool.begin().await.map_err(store_err)?;

    // Take the lock before any DDL, not after.
    //
    // `LOCK TABLE mem8_meta` cannot serialise this, because the table has to be
    // created before it can be locked -- and the statements that race are
    // themselves `CREATE`s. `CREATE ... IF NOT EXISTS` is not atomic against a
    // concurrent creator: both transactions check, both find nothing, and both
    // insert. For `CREATE EXTENSION vector` that surfaces as a duplicate key on
    // `pg_type_typname_nsp_index`, since both try to create the `vector` type.
    //
    // A transaction-scoped advisory lock is held on a bare integer, so it needs
    // no table to exist first and is released on commit or rollback. The
    // constant is arbitrary but must not change: it is the shared name every
    // mem8 process agrees to contend on.
    const MIGRATION_LOCK: i64 = 0x6d65_6d38_0001;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(MIGRATION_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;

    // `sqlx::query` rather than `raw_sql` throughout this function: `raw_sql`
    // produces a future that is not provably `Send`, which makes `connect`
    // itself non-`Send` and unusable from `tokio::spawn`. Each statement is
    // issued separately as a result.
    for statement in SCHEMA {
        sqlx::query(statement)
            .execute(&mut *tx)
            .await
            .map_err(store_err)?;
    }

    sqlx::query("CREATE TABLE IF NOT EXISTS mem8_meta (schema_version INT NOT NULL)")
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;

    let found: Option<i32> = sqlx::query_scalar("SELECT schema_version FROM mem8_meta LIMIT 1")
        .fetch_optional(&mut *tx)
        .await
        .map_err(store_err)?;

    // No row means either a brand-new database or one created before
    // versioning existed. Both are version 1: `SCHEMA` has just run, so the
    // v1 table shape is present either way.
    let found = found.unwrap_or(1);

    if found > PG_SCHEMA_VERSION {
        return Err(Mem8Error::Migration {
            found,
            expected: PG_SCHEMA_VERSION,
        });
    }

    if found < 2 {
        // Guard the destructive step. The column is expected to be NULL in
        // every row -- no code path has ever written it -- but "expected" is
        // not "verified", and dropping a column with data in it is not
        // recoverable. Check before dropping, not after.
        let populated: i64 =
            sqlx::query_scalar("SELECT count(*) FROM memories WHERE embedding IS NOT NULL")
                .fetch_one(&mut *tx)
                .await
                .map_err(store_err)?;

        if populated > 0 {
            return Err(Mem8Error::Store(format!(
                "cannot migrate to schema 2: {populated} rows have a non-NULL `embedding`, \
                 but the v1 column is a BYTEA placeholder that mem8 never writes. \
                 Back up the table and clear that column before upgrading."
            )));
        }

        sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                Mem8Error::Store(format!(
                    "could not enable the pgvector extension: {e}. \
                     mem8 needs `CREATE EXTENSION vector`, which requires a superuser or an \
                     owner of the database; run it once as that user, or use an image that \
                     ships pgvector such as pgvector/pgvector:pg16."
                ))
            })?;

        for statement in MIGRATE_V2 {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
    }

    // One row, always. DELETE-then-INSERT rather than UPDATE so a database
    // that had no row ends up with exactly one.
    sqlx::query("DELETE FROM mem8_meta")
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;
    sqlx::query("INSERT INTO mem8_meta (schema_version) VALUES ($1)")
        .bind(PG_SCHEMA_VERSION)
        .execute(&mut *tx)
        .await
        .map_err(store_err)?;

    tx.commit().await.map_err(store_err)?;
    Ok(())
}

/// Builds a store error that names the offending column and value, so a
/// corrupt row surfaces as a diagnosable error instead of being silently
/// replaced with a fabricated default. Postgres's typed columns (uuid,
/// timestamptz, text[]) make most row corruption impossible at the driver
/// level; `kind` is the one column stored as free text, so it is the one
/// case that needs this.
fn column_parse_error(
    column: &'static str,
    bad_value: &str,
    source: impl std::fmt::Display,
) -> Mem8Error {
    Mem8Error::Store(format!("invalid {column} value {bad_value:?}: {source}"))
}

/// An embedding in the form sqlx binds to a `vector` column.
///
/// Both builds must bind something Postgres will accept *as a vector*.
/// `Option::<Vec<f32>>::None` does not qualify: sqlx sends it typed as
/// `real[]`, and `COALESCE(real[], vector)` is an error that breaks every
/// `update` — including updates that never touch an embedding. The SQL casts
/// this parameter to `vector` explicitly for the same reason.
///
/// With the feature off there is no embedder and no `pgvector` crate, so the
/// only representable value is NULL. It is bound through `Option<&str>` so the
/// cast in the SQL is what gives it a type.
#[cfg(feature = "semantic")]
fn to_pgvector(embedding: Option<&[f32]>) -> Option<pgvector::Vector> {
    // `Vector: From<Vec<f32>>`, not `From<&[f32]>`, so the slice is copied.
    embedding.map(|e| pgvector::Vector::from(e.to_vec()))
}

#[cfg(not(feature = "semantic"))]
fn to_pgvector(_embedding: Option<&[f32]>) -> Option<&'static str> {
    None
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
        superseded_by: None,
        invalid_at: None,
    })
}

#[async_trait]
impl Store for PgStore {
    async fn add(&self, new: NewMemory) -> Result<Memory> {
        // INSERT ... RETURNING * (mirroring update()'s pattern) so the
        // returned Memory is built from what Postgres actually stored,
        // rather than from the Rust-side value before insertion. The
        // in-memory `now` carries nanosecond precision but the TIMESTAMPTZ
        // column only holds microseconds; reading the row back is what
        // makes add()'s return value match what a later get() produces.
        let now = Utc::now();
        let id = Uuid::new_v4();

        let row = sqlx::query(
            "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at, embedding)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector)
             RETURNING *",
        )
        .bind(id)
        .bind(&new.project)
        .bind(new.kind.to_string())
        .bind(&new.content)
        .bind(&new.tags)
        .bind(now)
        .bind(now)
        .bind(to_pgvector(new.embedding.as_deref()))
        .fetch_one(&self.pool)
        .await
        .map_err(store_err)?;

        row_to_memory(&row)
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
                embedding = COALESCE($4::vector, embedding),
                updated_at = $5
             WHERE id = $6
             RETURNING *",
        )
        .bind(update.content)
        .bind(update.kind.map(|k| k.to_string()))
        .bind(update.tags)
        .bind(to_pgvector(update.embedding.as_deref()))
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

    #[cfg(feature = "semantic")]
    async fn vector_search(&self, query: VectorQuery) -> Result<Vec<SearchHit>> {
        // `<=>` is pgvector's cosine *distance*: 0 is identical, 2 is opposite.
        // The HNSW index is built for exactly this operator, so ordering by it
        // ascending is what makes the index usable.
        //
        // `embedding IS NOT NULL` is not redundant with the ordering: NULLs
        // sort last under ASC, but they would still occupy result slots and
        // push real matches out of a limited set.
        let rows = sqlx::query(
            "SELECT *, embedding <=> $1 AS distance
             FROM memories
             WHERE embedding IS NOT NULL
               AND ($2::bool OR project = $3)
               AND ($4::text IS NULL OR kind = $4)
               AND ($5::text[] = '{}' OR tags @> $5)
             ORDER BY distance ASC
             LIMIT $6",
        )
        .bind(pgvector::Vector::from(query.embedding.clone()))
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
                let distance: f64 = row.get("distance");
                Ok(SearchHit {
                    memory: row_to_memory(row)?,
                    // Report similarity, not distance. Every other score in
                    // mem8 is higher-is-better, and a caller merging these
                    // lists must not have to know which way each one points.
                    score: 1.0 - distance,
                })
            })
            .collect()
    }

    /// Without the `semantic` feature there is no embedder, so nothing was ever
    /// stored to search against. The column exists but is uniformly NULL.
    #[cfg(not(feature = "semantic"))]
    async fn vector_search(&self, _query: VectorQuery) -> Result<Vec<SearchHit>> {
        Err(Mem8Error::Unsupported {
            feature: "semantic search".into(),
            backend: "this build (compiled without the `semantic` feature)".into(),
        })
    }

    async fn missing_embeddings(&self, limit: usize) -> Result<Vec<Memory>> {
        let rows = sqlx::query(
            "SELECT * FROM memories
             WHERE embedding IS NULL
             ORDER BY created_at ASC
             LIMIT $1",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(store_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    async fn set_embedding(&self, id: Uuid, embedding: &[f32]) -> Result<()> {
        // Deliberately does not touch `updated_at`: backfilling an index is
        // not an edit to the memory, and moving the timestamp would misreport
        // when the user last changed it.
        let result = sqlx::query("UPDATE memories SET embedding = $1::vector WHERE id = $2")
            .bind(to_pgvector(Some(embedding)))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;

        if result.rows_affected() == 0 {
            return Err(Mem8Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Temporarily unsupported: the real implementation arrives with the
    /// Postgres supersession columns. An error rather than a panic, so the
    /// intermediate state is visible to a caller instead of taking the process
    /// down.
    async fn supersede(&self, _old: Uuid, _new: Option<Uuid>, _at: DateTime<Utc>) -> Result<()> {
        Err(Mem8Error::Unsupported {
            feature: "supersession".into(),
            backend: "the Postgres backend".into(),
        })
    }
}
