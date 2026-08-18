//! Schema-version guard for the Postgres backend.
//!
//! `SqliteStore` has refused a too-new database since v1 via `PRAGMA
//! user_version`. `PgStore` gained the equivalent only when the vector column
//! made Postgres migrations real. These tests cover all three branches of that
//! guard: fresh database, forward migration, and refusal.
//!
//! Opt-in, like the Postgres contract suite: set `MEM8_TEST_PG`. A plain
//! `cargo test` with no database server must pass.

use mem8::error::Mem8Error;
use mem8::model::{Kind, NewMemory};
use mem8::store::postgres::{PgStore, PG_SCHEMA_VERSION};
use mem8::store::Store;
use sqlx::postgres::PgPoolOptions;

/// A private database for one test, or a skip when Postgres is not configured.
///
/// These tests operate on schema-global state -- dropping tables, creating the
/// `vector` extension, rewriting the version row. Sharing one database means
/// each test tears down objects the others are mid-way through using, and
/// `cargo test` runs them in parallel by default. Every test therefore gets a
/// database of its own, created and dropped around it.
macro_rules! scratch_db {
    () => {
        match std::env::var("MEM8_TEST_PG") {
            Ok(u) => ScratchDb::create(&u).await,
            Err(_) => {
                eprintln!("skipping: MEM8_TEST_PG not set");
                return;
            }
        }
    };
}

/// A throwaway database, dropped when the test ends.
struct ScratchDb {
    admin_url: String,
    name: String,
    url: String,
}

impl ScratchDb {
    async fn create(admin_url: &str) -> Self {
        let name = format!("mem8_mig_{}", uuid::Uuid::new_v4().simple());

        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(admin_url)
            .await
            .unwrap();
        // CREATE DATABASE cannot run inside a transaction block, so this is a
        // bare statement on its own connection.
        sqlx::raw_sql(&format!("CREATE DATABASE {name}"))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        // Swap the database name in the connection URL, keeping credentials.
        let url = match admin_url.rsplit_once('/') {
            Some((prefix, _)) => format!("{prefix}/{name}"),
            None => panic!("MEM8_TEST_PG must include a database name: {admin_url}"),
        };

        Self {
            admin_url: admin_url.to_string(),
            name,
            url,
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    /// Drop the database. Explicit rather than in `Drop`, because dropping
    /// needs async and must happen after every pool to it has closed.
    async fn cleanup(self) {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.admin_url)
            .await
            .unwrap();
        // Any lingering connection would block the drop; FORCE closes them.
        sqlx::raw_sql(&format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
}

/// The v1 schema, exactly as it shipped: the original table with `embedding`
/// as an unused BYTEA placeholder and no `mem8_meta`.
const V1_SCHEMA: &str = "CREATE TABLE memories (
    id          UUID PRIMARY KEY,
    project     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags        TEXT[] NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL,
    embedding   BYTEA
);";

async fn recorded_version(store: &PgStore) -> i32 {
    sqlx::query_scalar("SELECT schema_version FROM mem8_meta")
        .fetch_one(store.pool_for_tests())
        .await
        .unwrap()
}

/// The type Postgres reports for `memories.embedding`. Proves the column really
/// became a pgvector column rather than staying the BYTEA placeholder.
async fn embedding_type(store: &PgStore) -> String {
    sqlx::query_scalar(
        "SELECT format_type(a.atttypid, a.atttypmod)
         FROM pg_attribute a
         WHERE a.attrelid = 'memories'::regclass
           AND a.attname = 'embedding'
           AND NOT a.attisdropped",
    )
    .fetch_one(store.pool_for_tests())
    .await
    .unwrap()
}

/// The type Postgres reports for one `memories` column, or `None` when the
/// column does not exist. Proves a migration added a real column of the
/// intended type rather than the code merely reporting a default.
async fn column_type(store: &PgStore, column: &str) -> Option<String> {
    sqlx::query_scalar(
        "SELECT format_type(a.atttypid, a.atttypmod)
         FROM pg_attribute a
         WHERE a.attrelid = 'memories'::regclass
           AND a.attname = $1
           AND NOT a.attisdropped",
    )
    .bind(column)
    .fetch_optional(store.pool_for_tests())
    .await
    .unwrap()
}

#[tokio::test]
async fn fresh_database_is_created_at_the_current_version() {
    let db = scratch_db!();

    let store = PgStore::connect(db.url()).await.unwrap();
    assert_eq!(recorded_version(&store).await, PG_SCHEMA_VERSION);
    assert_eq!(
        embedding_type(&store).await,
        "vector(384)",
        "a fresh database must get the real vector column, not the BYTEA placeholder"
    );

    drop(store);
    db.cleanup().await;
}

#[tokio::test]
async fn connecting_twice_is_idempotent() {
    let db = scratch_db!();

    let first = PgStore::connect(db.url()).await.unwrap();
    let added = first
        .add(NewMemory {
            project: "p1".into(),
            kind: Kind::Fact,
            content: "survives a reconnect".into(),
            tags: vec![],
            ..Default::default()
        })
        .await
        .unwrap();

    // The second connect must not re-run the migration -- `ALTER TABLE ... ADD
    // COLUMN` would fail on an existing column, and a second `mem8_meta` row
    // would make the version ambiguous.
    let second = PgStore::connect(db.url()).await.unwrap();
    assert_eq!(recorded_version(&second).await, PG_SCHEMA_VERSION);
    assert_eq!(
        second.get(added.id).await.unwrap().content,
        "survives a reconnect"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mem8_meta")
        .fetch_one(second.pool_for_tests())
        .await
        .unwrap();
    assert_eq!(rows, 1, "mem8_meta must hold exactly one row");

    drop((first, second));
    db.cleanup().await;
}

/// Two mem8 processes starting at once against a fresh database.
///
/// This is not hypothetical: an agent session and a `mem8` CLI invocation can
/// begin together, and both call `connect`. The first version of the guard
/// locked `mem8_meta` -- which cannot work, because the racing statements are
/// the `CREATE`s that must run before there is a table to lock. It failed here
/// with a duplicate key on `pg_type_typname_nsp_index`, both connections having
/// tried to create the `vector` type.
#[tokio::test]
async fn concurrent_first_connections_do_not_race() {
    let db = scratch_db!();
    let url = db.url().to_string();

    let attempts: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move {
                let url: String = url;
                PgStore::connect(url.as_str())
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        })
        .collect();

    for (i, attempt) in attempts.into_iter().enumerate() {
        let result = attempt.await.expect("connect task must not panic");
        assert!(
            result.is_ok(),
            "concurrent connect {i} failed: {:?}",
            result.err()
        );
    }

    let store = PgStore::connect(&url).await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM mem8_meta")
        .fetch_one(store.pool_for_tests())
        .await
        .unwrap();
    assert_eq!(
        rows, 1,
        "racing migrations must still leave exactly one version row"
    );

    drop(store);
    db.cleanup().await;
}

#[tokio::test]
async fn a_v1_database_migrates_forward_keeping_its_rows() {
    let db = scratch_db!();

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db.url())
        .await
        .unwrap();
    sqlx::raw_sql(V1_SCHEMA).execute(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at, embedding)
         VALUES (gen_random_uuid(), 'p1', 'decision', 'written under schema v1', '{}',
                 now(), now(), NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = PgStore::connect(db.url()).await.unwrap();

    assert_eq!(recorded_version(&store).await, PG_SCHEMA_VERSION);
    assert_eq!(embedding_type(&store).await, "vector(384)");

    // A v1 database skips no step: MIGRATE_V2 and MIGRATE_V3 both run, in
    // order, inside the one transaction. Asserting the v3 columns here makes
    // that explicit rather than incidental -- a `found < 3` guard written as
    // `found == 2` would still pass every other test in this file.
    assert_eq!(
        column_type(&store, "superseded_by").await.as_deref(),
        Some("uuid")
    );
    assert_eq!(
        column_type(&store, "invalid_at").await.as_deref(),
        Some("timestamp with time zone")
    );

    // The migration drops a column; it must not drop the row with it.
    let all = store.all().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].content, "written under schema v1");

    drop(store);
    db.cleanup().await;
}

/// Updating a memory must work in a build with no `semantic` feature.
///
/// The v2 column is `vector(384)` regardless of how mem8 was compiled, so a
/// default build still has to bind and COALESCE against it. Binding a plain
/// `Option<Vec<f32>>` instead sends `real[]`, and `COALESCE(real[], vector)`
/// fails — which broke *every* update, not just ones involving embeddings.
///
/// Deliberately not behind `#[cfg(feature = "semantic")]`: the point is that
/// the default build works.
#[tokio::test]
async fn update_works_against_a_vector_column() {
    let db = scratch_db!();
    let store = PgStore::connect(db.url()).await.unwrap();

    let added = store
        .add(NewMemory {
            project: "p1".into(),
            kind: Kind::Fact,
            content: "before".into(),
            tags: vec![],
            ..Default::default()
        })
        .await
        .unwrap();

    let updated = store
        .update(
            added.id,
            mem8::model::MemoryUpdate {
                content: Some("after".into()),
                ..Default::default()
            },
        )
        .await
        .expect("update must work whether or not this build has embeddings");

    assert_eq!(updated.content, "after");

    drop(store);
    db.cleanup().await;
}

#[tokio::test]
async fn a_newer_database_is_refused() {
    let db = scratch_db!();

    let store = PgStore::connect(db.url()).await.unwrap();
    drop(store);

    // Simulate a database written by a future mem8.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db.url())
        .await
        .unwrap();
    sqlx::query("UPDATE mem8_meta SET schema_version = $1")
        .bind(PG_SCHEMA_VERSION + 1)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    match PgStore::connect(db.url()).await {
        Ok(_) => panic!("a database newer than the binary must be refused, not opened"),
        Err(Mem8Error::Migration { found, expected }) => {
            assert_eq!(found, PG_SCHEMA_VERSION + 1);
            assert_eq!(expected, PG_SCHEMA_VERSION);
        }
        Err(e) => panic!("expected a Migration error, got: {e}"),
    }

    db.cleanup().await;
}

#[tokio::test]
async fn a_v1_database_with_data_in_the_placeholder_column_is_not_silently_dropped() {
    let db = scratch_db!();

    // mem8 never writes `embedding` in v1, so this cannot arise from mem8
    // itself -- but the migration drops that column, and dropping a column
    // that turns out to hold data is unrecoverable. The guard must refuse
    // rather than destroy.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db.url())
        .await
        .unwrap();
    sqlx::raw_sql(V1_SCHEMA).execute(&pool).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at, embedding)
         VALUES (gen_random_uuid(), 'p1', 'fact', 'has bytes in embedding', '{}',
                 now(), now(), '\\x00010203'::bytea);",
    )
    .execute(&pool)
    .await
    .unwrap();

    let err = match PgStore::connect(db.url()).await {
        Ok(_) => panic!("migration must refuse to drop a column holding data"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("embedding"),
        "error should name the column, got: {err}"
    );

    // The transaction must have rolled back: the row and its bytes survive.
    let surviving: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memories WHERE embedding IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        surviving, 1,
        "a refused migration must leave the data intact"
    );

    pool.close().await;
    db.cleanup().await;
}

/// A v2 database — one an older binary left behind — must gain the
/// supersession columns without disturbing its rows.
///
/// Built by running the current migration and then winding the recorded
/// version back and dropping the v3 columns, rather than by pasting a v2
/// `CREATE TABLE`: the v2 shape includes the pgvector column and its HNSW
/// index, and re-spelling that here would drift from `SCHEMA`/`MIGRATE_V2`
/// the moment either changes.
#[tokio::test]
async fn v2_to_v3_preserves_every_row_as_live() {
    let db = scratch_db!();

    let store = PgStore::connect(db.url()).await.unwrap();
    let added = store
        .add(NewMemory {
            project: "p1".into(),
            kind: Kind::Fact,
            content: "written under schema v2".into(),
            tags: vec!["t1".into()],
            ..Default::default()
        })
        .await
        .unwrap();
    drop(store);

    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(db.url())
        .await
        .unwrap();
    sqlx::raw_sql(
        "ALTER TABLE memories DROP COLUMN IF EXISTS superseded_by;
         ALTER TABLE memories DROP COLUMN IF EXISTS invalid_at;
         UPDATE mem8_meta SET schema_version = 2;",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    // Reconnecting runs v2 -> v3.
    let store = PgStore::connect(db.url()).await.unwrap();
    assert_eq!(recorded_version(&store).await, PG_SCHEMA_VERSION);

    // Assert the columns are physically back. Without this the test passes
    // vacuously against a binary that reports `None` from a hardcoded literal
    // rather than from a column it actually read.
    assert_eq!(
        column_type(&store, "superseded_by").await.as_deref(),
        Some("uuid")
    );
    assert_eq!(
        column_type(&store, "invalid_at").await.as_deref(),
        Some("timestamp with time zone"),
        "invalid_at must be TIMESTAMPTZ; TEXT would give Postgres a \
         text-comparison fragility its other timestamps do not have"
    );

    let all = store.all().await.unwrap();
    assert_eq!(all.len(), 1, "the migration must not lose rows");
    assert_eq!(all[0].id, added.id);
    assert_eq!(all[0].content, "written under schema v2");
    assert_eq!(all[0].tags, vec!["t1".to_string()]);

    // Pre-existing rows migrate to live, not to some default invalidation.
    assert_eq!(all[0].superseded_by, None);
    assert_eq!(all[0].invalid_at, None);

    // And a live row stays findable by search, which now carries a temporal
    // predicate that a botched migration would make exclude everything.
    let found = store.get(added.id).await.unwrap();
    assert_eq!(found.invalid_at, None);

    drop(store);
    db.cleanup().await;
}
