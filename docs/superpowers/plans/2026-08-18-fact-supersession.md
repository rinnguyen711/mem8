# Fact Supersession Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a memory record that it replaced another, so a project that changes its mind returns only what is currently true — while keeping superseded facts retrievable by id and answerable at a past time.

**Architecture:** Two nullable columns on `memories` (`superseded_by`, `invalid_at`), where `invalid_at` alone determines whether a memory is live. Search gains one predicate with three modes (live / all / as-of). The write path takes an explicit `supersedes` uuid from the agent rather than inferring it, writes the new memory first and invalidates the old second, and validates before spending an embedding. Export/import round-trips both fields, remapping the successor pointer to freshly imported ids.

**Tech Stack:** Rust, `async_trait`, `sqlx` (Postgres), `rusqlite` (SQLite), `chrono`, `uuid`, `rmcp` + `schemars` for the MCP tool surface.

**Scope:** Items 1–4 of `docs/superpowers/specs/2026-08-17-fact-supersession-design.md`. Semantic dedup (item 5, gated on threshold calibration) and recency decay (item 6) are deliberately out — they are independently shippable and can slip without leaving anything half-built.

---

## Background for the implementer

You need three facts about this codebase before starting.

**1. The migration hazard is real and already diagnosed.** `src/store/sqlite.rs:71` (`init`) runs `CREATE TABLE IF NOT EXISTS`, then bumps `PRAGMA user_version` — with no `ALTER TABLE` anywhere. If you bump `SCHEMA_VERSION` to 2 without adding a migration step, an existing v1 database gets stamped as v2 while still missing the new columns, and every later query fails on a missing column. Postgres does not have this problem: `migrate` in `src/store/postgres.rs:103` already has a versioned step list (`MIGRATE_V2`) inside a transaction-scoped advisory lock. Copy that shape for SQLite; extend it for Postgres.

**2. `NewMemory` has no id, and import needs one.** `from_markdown` returns `Vec<NewMemory>`, and `NewMemory` carries no identifier — import deliberately creates fresh rows so it cannot collide with existing ids. But remapping `superseded_by` requires knowing each section's *original* heading uuid to build the old→new mapping. So `from_markdown`'s return type must change to carry the parsed uuid alongside the `NewMemory`. Task 8 does this. Do not try to smuggle it through `NewMemory`.

**3. SQLite's FTS triggers fire on every UPDATE.** `memories_au` is an unconditional `AFTER UPDATE ON memories` trigger that deletes and re-inserts the FTS row. `supersede` issues an `UPDATE`, so it rewrites the FTS entry every time. This is harmless — `content` is unchanged, so the row is rewritten identically — and we are deliberately **not** narrowing the trigger, because that would mean changing existing schema DDL for no behavioural gain. Task 3 adds a comment recording this.

**Baseline:** `cargo test` is green at 114 passing before you start. The Postgres suites (`tests/pg_migration.rs`, `tests/pg_vector.rs`) report 0 tests without a live database and are expected to.

**Running Postgres tests:** several tasks have Postgres halves that only execute with `MEM8_TEST_PG` set. If you cannot run them, say so explicitly in your task report rather than reporting a pass — the whole point of the contract suite is that both backends are verified.

---

## File structure

| File | Responsibility | Change |
|---|---|---|
| `src/model.rs` | `Memory` fields; `SearchQuery`/`VectorQuery` filter fields | Modify |
| `src/store/mod.rs` | `Store::supersede`; `MemStore` filtering must mirror real backends | Modify |
| `src/store/sqlite.rs` | v1→v2 migration; column read/write; search predicate | Modify |
| `src/store/postgres.rs` | v2→v3 migration; column read/write; search predicate | Modify |
| `src/core.rs` | `supersedes` param, validation, ordering; `find_duplicate` filter | Modify |
| `src/mcp/mod.rs` | `supersedes` on `AddMemoryParams`; `as_of`/`include_superseded` on search; mutual-exclusion rejection | Modify |
| `src/cli/markdown.rs` | Two optional markdown fields; parsed-uuid return type | Modify |
| `src/cli/mod.rs` | Import remapping pass | Modify |
| `tests/store_contract.rs` | Supersession semantics, verified once for both backends | Modify |
| `tests/pg_migration.rs` | v2→v3 preserves rows as live | Modify |
| `tests/sqlite_migration.rs` | v1→v2 preserves rows as live | Create |
| `tests/cli_roundtrip.rs` | Export/import round trip with remapping | Modify |
| `README.md` | Tool table, supersession section, format fields | Modify |

---

## Task 1: Model fields

**Files:**
- Modify: `src/model.rs:57-70` (`Memory`)
- Test: `src/model.rs` (inline `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/model.rs`:

```rust
#[test]
fn live_memory_omits_supersession_fields_from_json() {
    let now = Utc::now();
    let m = Memory {
        id: Uuid::new_v4(),
        project: "p".into(),
        kind: Kind::Decision,
        content: "c".into(),
        tags: vec![],
        created_at: now,
        updated_at: now,
        embedding: None,
        superseded_by: None,
        invalid_at: None,
    };

    let json = serde_json::to_string(&m).unwrap();
    assert!(
        !json.contains("superseded_by"),
        "a live memory must serialize exactly as before, got: {json}"
    );
    assert!(!json.contains("invalid_at"), "got: {json}");
}

#[test]
fn superseded_memory_includes_both_fields_in_json() {
    let now = Utc::now();
    let successor = Uuid::new_v4();
    let m = Memory {
        id: Uuid::new_v4(),
        project: "p".into(),
        kind: Kind::Decision,
        content: "c".into(),
        tags: vec![],
        created_at: now,
        updated_at: now,
        embedding: None,
        superseded_by: Some(successor),
        invalid_at: Some(now),
    };

    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains(&successor.to_string()));
    assert!(json.contains("invalid_at"));
}
```

These need `use chrono::Utc;` at the top of `mod tests` if not already present.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib model::tests 2>&1 | tail -20`
Expected: FAIL to **compile** — `struct Memory has no field named superseded_by`. A compile failure is the correct red state here; the field does not exist yet.

- [ ] **Step 3: Write minimal implementation**

In `src/model.rs`, add to `Memory` after `embedding`:

```rust
    /// The memory that replaced this one, if its successor is known.
    ///
    /// Written only alongside `invalid_at`: a successor with no invalidation
    /// time is incoherent and must never be stored. The converse is legitimate
    /// — `invalid_at` set with `superseded_by` NULL means the memory is known
    /// dead but its replacement is unknown, which import produces when a file
    /// names a successor it does not contain.
    ///
    /// The invariant is enforced in the store layer rather than by a database
    /// constraint, so both backends behave identically and the rule lives next
    /// to the code that applies it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<Uuid>,
    /// When this memory stopped being true. `None` means it is still live, and
    /// this field alone is what search filters on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_at: Option<DateTime<Utc>>,
```

`skip_serializing_if` matches `embedding`, so a live memory serializes exactly as it does today and no existing consumer of the tool output sees a new field.

- [ ] **Step 4: Fix every `Memory` construction site**

The new fields have no `Default`, so every literal breaks. Find them:

```bash
cargo build 2>&1 | grep -E "^error" | head -20
```

Add `superseded_by: None, invalid_at: None,` to each. Known sites: `src/store/mod.rs` (`MemStore::add`), `src/store/sqlite.rs` (row mapping), `src/store/postgres.rs` (row mapping), `src/cli/markdown.rs` (test helper `memory`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib model::tests 2>&1 | tail -10`
Expected: PASS, both new tests.

- [ ] **Step 6: Full suite still green**

Run: `cargo test 2>&1 | grep -E "^test result" `
Expected: every line `ok`, total 116 passing (114 baseline + 2 new).

- [ ] **Step 7: Commit**

```bash
git add src/model.rs src/store/mod.rs src/store/sqlite.rs src/store/postgres.rs src/cli/markdown.rs
git commit -m "feat(model): add superseded_by and invalid_at to Memory"
```

---

## Task 2: Query filter fields

**Files:**
- Modify: `src/model.rs` (`SearchQuery`, `VectorQuery`)
- Test: `src/model.rs` (inline `mod tests`)

Both query types gain the fields, not only the keyword one. A semantic search that ignored supersession would surface dead facts that keyword search correctly hides — the same reasoning already recorded in `VectorQuery`'s doc comment about `project`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn query_defaults_hide_superseded_and_set_no_as_of() {
    let q = SearchQuery {
        text: "x".into(),
        project: None,
        global: false,
        kind: None,
        tags: vec![],
        limit: 10,
        include_superseded: false,
        as_of: None,
    };
    assert!(!q.include_superseded);
    assert!(q.as_of.is_none());

    let v = VectorQuery {
        embedding: vec![0.0; 3],
        project: None,
        global: false,
        kind: None,
        tags: vec![],
        limit: 10,
        include_superseded: false,
        as_of: None,
    };
    assert!(!v.include_superseded);
    assert!(v.as_of.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib model::tests::query_defaults 2>&1 | tail -20`
Expected: FAIL to compile — `SearchQuery has no field named include_superseded`.

- [ ] **Step 3: Write minimal implementation**

Add to **both** `SearchQuery` and `VectorQuery` in `src/model.rs`:

```rust
    /// Return superseded memories alongside live ones. Defaults to false:
    /// discovery should surface what is currently true.
    pub include_superseded: bool,
    /// Answer as of a past instant — what was believed then.
    ///
    /// Mutually exclusive with `include_superseded` and rejected together at
    /// the tool boundary: `as_of` already specifies exactly which rows count,
    /// so combining them is a contradiction rather than a refinement. Stores
    /// resolve it by letting `as_of` win rather than trusting the boundary,
    /// so a direct caller cannot produce a nonsense result.
    pub as_of: Option<DateTime<Utc>>,
```

- [ ] **Step 4: Fix every construction site**

```bash
cargo build --all-targets 2>&1 | grep -E "^error" | head -30
```

Add `include_superseded: false, as_of: None,` to each. Known sites: `src/core.rs` (`find_duplicate`, `search`, `vector_hits`), `src/store/mod.rs` (inline tests), `tests/store_contract.rs` (`query` helper), plus any `..query(...)` spread sites which need no change.

Consider adding `#[derive(Default)]`-style helpers only if the codebase already does so — it does not for these types, so update sites explicitly.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib model::tests::query_defaults 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Full suite green**

Run: `cargo test 2>&1 | grep -E "^test result"`
Expected: all `ok`, 117 passing.

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "feat(model): add supersession filters to SearchQuery and VectorQuery"
```

---

## Task 3: SQLite migration v1 to v2

**Files:**
- Modify: `src/store/sqlite.rs:12` (`SCHEMA_VERSION`), `src/store/sqlite.rs:71` (`init`)
- Test: `tests/sqlite_migration.rs` (create)

This is the task the background section warns about. The version bump and the `ALTER TABLE`s must land in the same commit, and both must run in one transaction so an interrupted upgrade leaves a v1 database rather than a half-migrated one.

- [ ] **Step 1: Write the failing test**

Create `tests/sqlite_migration.rs`:

```rust
use mem8::model::{Kind, NewMemory};
use mem8::store::sqlite::SqliteStore;
use mem8::store::Store;

/// A v1 database — the shape that shipped before supersession — with one row
/// already in it. Written by hand rather than by an old binary, because the
/// point is to prove the migration runs, not to prove rusqlite works.
fn write_v1_database(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE memories (
            id          TEXT PRIMARY KEY,
            project     TEXT NOT NULL,
            kind        TEXT NOT NULL,
            content     TEXT NOT NULL,
            tags        TEXT NOT NULL DEFAULT '[]',
            created_at  TEXT NOT NULL,
            updated_at  TEXT NOT NULL,
            embedding   BLOB
        );
        INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at)
        VALUES (
            '7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f', 'p1', 'decision',
            'we chose sqlite', '[]',
            '2026-08-01T00:00:00+00:00', '2026-08-01T00:00:00+00:00'
        );
        PRAGMA user_version = 1;
        "#,
    )
    .unwrap();
}

#[tokio::test]
async fn opening_a_v1_database_migrates_and_keeps_rows_live() {
    let dir = std::env::temp_dir().join(format!("mem8-mig-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v1.db");
    write_v1_database(&path);

    let store = SqliteStore::open(&path).unwrap();

    // The pre-existing row survived and is live.
    let all = store.all().await.unwrap();
    assert_eq!(all.len(), 1, "migration must not lose rows");
    assert_eq!(all[0].content, "we chose sqlite");
    assert!(
        all[0].invalid_at.is_none() && all[0].superseded_by.is_none(),
        "every memory that exists today must stay live after upgrade"
    );

    // The new columns are real: a write that uses them succeeds.
    let fresh = store
        .add(NewMemory {
            project: "p1".into(),
            kind: Kind::Decision,
            content: "we chose postgres".into(),
            tags: vec![],
            ..Default::default()
        })
        .await
        .unwrap();
    store
        .supersede(all[0].id, Some(fresh.id), chrono::Utc::now())
        .await
        .unwrap();
    assert!(store.get(all[0].id).await.unwrap().invalid_at.is_some());

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reopening_a_migrated_database_is_a_no_op() {
    let dir = std::env::temp_dir().join(format!("mem8-mig2-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v1.db");
    write_v1_database(&path);

    drop(SqliteStore::open(&path).unwrap());
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(store.all().await.unwrap().len(), 1);

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}
```

`rusqlite` must be available to the test target. Check `Cargo.toml`: if `rusqlite` is a non-dev dependency it is already usable from an integration test only if re-exported; otherwise add it under `[dev-dependencies]` with the same version and features as the main dependency.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test sqlite_migration 2>&1 | tail -20`
Expected: FAIL — no method named `supersede`, and once that is stubbed, a store error naming a missing `superseded_by` column.

- [ ] **Step 3: Add the migration**

In `src/store/sqlite.rs`, bump the constant:

```rust
pub const SCHEMA_VERSION: i32 = 2;
```

Add the new columns to the `SCHEMA` `CREATE TABLE` block so a brand-new database gets them directly (after `embedding BLOB`):

```
    embedding   BLOB,
    superseded_by TEXT,
    invalid_at    TEXT
```

Add the migration step list above `SqliteStore`:

```rust
/// Version 2: record that one fact replaced another.
///
/// Both columns are nullable and existing rows migrate to NULL/NULL, so every
/// memory that exists today stays live and keeps being returned.
const MIGRATE_V2: &[&str] = &[
    "ALTER TABLE memories ADD COLUMN superseded_by TEXT",
    "ALTER TABLE memories ADD COLUMN invalid_at TEXT",
];
```

Rewrite `init` so the ALTERs run before the version bump, in one transaction:

```rust
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
            if found < 2 {
                for statement in MIGRATE_V2 {
                    // A fresh database already has the columns from SCHEMA, so
                    // a duplicate-column error here is success, not failure.
                    match tx.execute(statement, []) {
                        Ok(_) => {}
                        Err(e) if e.to_string().contains("duplicate column name") => {}
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
```

Note `mut conn` — `Connection::transaction` needs `&mut self`. Update the two callers (`open`, `open_in_memory`) if they bind `conn` immutably.

- [ ] **Step 4: Read and write the new columns**

In the row-mapping function, read both columns. They are stored as RFC3339 text like `created_at`, so reuse whatever helper that uses:

```rust
    superseded_by: row
        .get::<_, Option<String>>("superseded_by")
        .map_err(store_err)?
        .map(|s| Uuid::parse_str(&s))
        .transpose()
        .map_err(|e| corrupt_row("superseded_by", &e.to_string()))?,
    invalid_at: row
        .get::<_, Option<String>>("invalid_at")
        .map_err(store_err)?
        .map(|s| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)))
        .transpose()
        .map_err(|e| corrupt_row("invalid_at", &e.to_string()))?,
```

Match the existing error-construction helper for corrupt rows — `src/store/postgres.rs` has one described as "names the offending column and value"; use the SQLite equivalent already in the file rather than inventing `corrupt_row` if a different name exists.

Ensure every `SELECT` in the file lists the new columns, or uses `*`.

- [ ] **Step 5: Implement `supersede`**

Add to `impl Store for SqliteStore`:

```rust
    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()> {
        // One statement sets both columns, so `invalid_at` is never set without
        // the successor being decided in the same write. `new` may be NULL --
        // known dead, successor unknown -- but the reverse (a successor with no
        // invalidation time) is incoherent and unreachable from here.
        //
        // This UPDATE fires the unconditional `memories_au` FTS trigger, which
        // deletes and re-inserts the row's FTS entry. Harmless — `content` is
        // unchanged, so the entry is rewritten identically — and narrowing the
        // trigger would mean changing shipped schema DDL for no behavioural
        // gain.
        let conn = self.conn.lock().unwrap();
        let changed = conn
            .execute(
                "UPDATE memories SET superseded_by = ?1, invalid_at = ?2 WHERE id = ?3",
                rusqlite::params![
                    new.map(|n| n.to_string()),
                    at.to_rfc3339(),
                    old.to_string()
                ],
            )
            .map_err(store_err)?;

        if changed == 0 {
            return Err(Mem8Error::NotFound(old.to_string()));
        }
        Ok(())
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --test sqlite_migration 2>&1 | tail -15`
Expected: PASS, 2 tests.

Note the ordering dependency: `supersede` must exist on the `Store` trait before this compiles, and the trait is defined in Task 4. Execute **Task 4 Step 1 first** (the trait signature plus the `MemStore` implementation), then return here. The signature is identical in both places:

```rust
async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()>;
```

Note `new: Option<Uuid>` — Task 4 explains why the successor is optional. Getting this right now avoids rewriting both backends at Task 11.

- [ ] **Step 7: Commit**

```bash
git add src/store/sqlite.rs tests/sqlite_migration.rs Cargo.toml
git commit -m "feat(sqlite): migrate to schema 2 with supersession columns"
```

---

## Task 4: Store trait and MemStore

**Files:**
- Modify: `src/store/mod.rs` (trait `Store`, `impl Store for MemStore`)
- Test: `src/store/mod.rs` (inline `mod tests`)

`MemStore` must gain the same filtering as the real backends, or the `core` unit tests drift from what the real backends do — which is the one failure mode a test double introduces that no amount of coverage catches.

- [ ] **Step 1: Add the trait method and MemStore implementation**

Do this step first if you are executing in order — Task 3 depends on it.

Add to `trait Store` in `src/store/mod.rs`:

```rust
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
    /// Returns `NotFound` if `old` does not exist. Validation of the
    /// *relationship* — same project, not already superseded — belongs to
    /// `core`, not here: the store records the fact, it does not police intent.
    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()>;
```

Add `use chrono::{DateTime, Utc};` to the imports (the file currently imports only `Utc`).

Implement for `MemStore`:

```rust
    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|m| m.id == old)
            .ok_or_else(|| Mem8Error::NotFound(old.to_string()))?;
        row.superseded_by = new;
        row.invalid_at = Some(at);
        Ok(())
    }
```

- [ ] **Step 2: Write the failing filter test**

```rust
#[tokio::test]
async fn search_hides_superseded_by_default() {
    let store = MemStore::new();
    let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
    let new = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
    store.supersede(old.id, Some(new.id), Utc::now()).await.unwrap();

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
    store.supersede(old.id, Some(new.id), Utc::now()).await.unwrap();

    let q = SearchQuery { include_superseded: true, ..query("sqlite", "p1") };
    assert_eq!(store.search(q).await.unwrap().len(), 2);
}

#[tokio::test]
async fn get_still_returns_a_superseded_memory() {
    let store = MemStore::new();
    let old = store.add(new_memory("p1", "use sqlite")).await.unwrap();
    let new = store.add(new_memory("p1", "use sqlite now")).await.unwrap();
    store.supersede(old.id, Some(new.id), Utc::now()).await.unwrap();

    // Hidden from discovery, not deleted. This distinction is the whole reason
    // supersession is not delete_memory.
    let got = store.get(old.id).await.unwrap();
    assert_eq!(got.content, "use sqlite");
    assert_eq!(got.superseded_by, Some(new.id));
}
```

The `query` helper in this module takes `(text, project)` — check its current signature and add the two new fields to it.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib store::tests 2>&1 | tail -20`
Expected: FAIL — `search_hides_superseded_by_default` finds the superseded memory, because `MemStore::search` does not filter yet.

- [ ] **Step 4: Add filtering to MemStore**

Write one predicate helper next to `MemStore`, used by both `search` and `vector_search` so they cannot diverge:

```rust
/// Whether a memory is visible under a query's temporal filters.
///
/// Three modes, one predicate:
/// - default: live only (`invalid_at IS NULL`)
/// - `include_superseded`: everything
/// - `as_of: T`: what was believed at T
fn visible_at(
    memory: &Memory,
    include_superseded: bool,
    as_of: Option<DateTime<Utc>>,
) -> bool {
    match as_of {
        Some(t) => {
            memory.created_at <= t && memory.invalid_at.is_none_or(|invalid| invalid > t)
        }
        None => include_superseded || memory.invalid_at.is_none(),
    }
}
```

Add to the filter chain in **both** `MemStore::search` and `MemStore::vector_search`:

```rust
            .filter(|m| visible_at(m, query.include_superseded, query.as_of))
```

Place it before `.take(query.limit)` in `search`, and inside the `filter_map` chain's preceding filters in `vector_search`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib store::tests 2>&1 | tail -10`
Expected: PASS, all 3 new tests.

- [ ] **Step 6: Commit**

```bash
git add src/store/mod.rs
git commit -m "feat(store): add supersede to the Store trait and MemStore filtering"
```

---

## Task 5: SQLite search filtering

**Files:**
- Modify: `src/store/sqlite.rs` (`search`, `vector_search`, `missing_embeddings` left alone)
- Test: `tests/store_contract.rs`

- [ ] **Step 1: Write the failing contract assertions**

Add to `run_contract` in `tests/store_contract.rs`, after the existing search assertions:

```rust
    // --- Supersession ---------------------------------------------------
    let old = store
        .add(new_memory("p1", Kind::Decision, "storage is sqlite", &[]))
        .await
        .unwrap();
    let replacement = store
        .add(new_memory("p1", Kind::Decision, "storage is postgres", &[]))
        .await
        .unwrap();
    let at = chrono::Utc::now();
    store.supersede(old.id, Some(replacement.id), at).await.unwrap();

    // Hidden from search by default.
    let live = store.search(query("storage")).await.unwrap();
    let live_ids: Vec<_> = live.iter().map(|h| h.memory.id).collect();
    assert!(!live_ids.contains(&old.id), "superseded must be hidden");
    assert!(live_ids.contains(&replacement.id));

    // Still retrievable in full by id.
    let fetched = store.get(old.id).await.unwrap();
    assert_eq!(fetched.superseded_by, Some(replacement.id));
    assert!(fetched.invalid_at.is_some());
    assert_eq!(fetched.content, "storage is sqlite");

    // include_superseded returns both.
    let both = store
        .search(SearchQuery { include_superseded: true, ..query("storage") })
        .await
        .unwrap();
    let both_ids: Vec<_> = both.iter().map(|h| h.memory.id).collect();
    assert!(both_ids.contains(&old.id) && both_ids.contains(&replacement.id));

    // Superseding a memory that does not exist is NotFound.
    assert!(store
        .supersede(uuid::Uuid::new_v4(), Some(replacement.id), at)
        .await
        .is_err());
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test store_contract 2>&1 | tail -20`
Expected: FAIL on `superseded must be hidden` — SQLite's `search` returns it.

- [ ] **Step 3: Add the predicate to SQLite's search**

`search` builds SQL with bound parameters. Add the temporal predicate to the `WHERE` clause the same way the existing `project`/`kind`/`tags` filters are added — follow the file's established pattern for conditional clauses rather than string-concatenating a new one differently.

The three modes:

```rust
    // Three modes, one predicate. `as_of` already specifies exactly which rows
    // count, so it takes precedence over `include_superseded`; the tool
    // boundary rejects a caller that sets both.
    match query.as_of {
        Some(t) => {
            clauses.push("m.created_at <= ?".to_string());
            params.push(Box::new(t.to_rfc3339()));
            clauses.push("(m.invalid_at IS NULL OR m.invalid_at > ?)".to_string());
            params.push(Box::new(t.to_rfc3339()));
        }
        None if !query.include_superseded => {
            clauses.push("m.invalid_at IS NULL".to_string());
        }
        None => {}
    }
```

Adapt the clause/param accumulation to whatever the function actually uses (it may use a `Vec<String>` of conditions and a `params_from_iter` call). Use the table alias the existing query uses — drop `m.` if there is none.

Timestamps are stored as RFC3339 text, and RFC3339 with a fixed `+00:00` offset compares correctly as a string — verified against a 200,000-pair fuzz during the Task 3-4 review, including nanosecond, microsecond, and millisecond boundaries. The `.` separator sorts below every digit, so a whole second correctly precedes any fractional value at the same second.

**This holds only for the `+00:00` form.** Bind `as_of` with plain `to_rfc3339()`. The `Z` form breaks ordering silently — `'Z'` (0x5A) > `'+'` (0x2B), so the same instant compares as *later* — and no existing test would catch it. Never use `to_rfc3339_opts(.., use_z = true)` for a value compared against these columns.

- [ ] **Step 4: Add the predicate to SQLite's vector_search**

Apply the identical predicate. If `vector_search` returns `Unsupported` on this backend (no pgvector), there is nothing to filter — check the function body. If it is unimplemented, leave a comment saying the predicate belongs here when it gains an implementation, and note it in the commit message.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test store_contract 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/store/sqlite.rs tests/store_contract.rs
git commit -m "feat(sqlite): hide superseded memories from search by default"
```

---

## Task 6: Postgres migration v2 to v3 and filtering

**Files:**
- Modify: `src/store/postgres.rs:15` (`PG_SCHEMA_VERSION`), `src/store/postgres.rs:103` (`migrate`), `search`, `vector_search`
- Test: `tests/pg_migration.rs`, `tests/store_contract.rs` (already extended in Task 5)

Postgres already has the right shape — a versioned step list inside a transaction-scoped advisory lock. This task extends it rather than inventing anything.

- [ ] **Step 1: Write the failing migration test**

Add to `tests/pg_migration.rs`, following the file's existing conventions for acquiring a test database (it is gated on an env var — reuse that gate exactly):

```rust
#[tokio::test]
async fn v2_to_v3_preserves_every_row_as_live() {
    let Some(url) = test_database_url() else { return };

    // Build a v2 database: run the real migration, then wind the recorded
    // version back and drop the v3 columns, which is the state an older
    // binary would have left behind.
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP TABLE IF EXISTS memories, mem8_meta")
        .execute(&pool)
        .await
        .unwrap();
    drop(PgStore::connect(&url).await.unwrap());

    sqlx::query(
        "INSERT INTO memories (id, project, kind, content, tags, created_at, updated_at)
         VALUES ($1, 'p1', 'decision', 'we chose sqlite', '{}', now(), now())",
    )
    .bind(uuid::Uuid::new_v4())
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("ALTER TABLE memories DROP COLUMN IF EXISTS superseded_by")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE memories DROP COLUMN IF EXISTS invalid_at")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE mem8_meta SET schema_version = 2")
        .execute(&pool)
        .await
        .unwrap();

    // Reopening runs v2 -> v3.
    let store = PgStore::connect(&url).await.unwrap();
    let all = store.all().await.unwrap();
    assert_eq!(all.len(), 1, "migration must not lose rows");
    assert!(
        all[0].invalid_at.is_none() && all[0].superseded_by.is_none(),
        "existing rows must migrate to live"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `MEM8_TEST_PG=1 cargo test --test pg_migration 2>&1 | tail -20`
(Substitute the actual gate variable and connection URL the file already uses.)
Expected: FAIL — the reopen errors on a missing column, or the columns never come back.

If you have no Postgres available, stop and report that this task's verification could not be run. Do not mark it passing.

- [ ] **Step 3: Add the migration step**

```rust
pub const PG_SCHEMA_VERSION: i32 = 3;
```

Add the new columns to the `SCHEMA` `CREATE TABLE` block, after `embedding BYTEA`:

```
        superseded_by UUID,
        invalid_at    TIMESTAMPTZ
```

Add the step list next to `MIGRATE_V2`:

```rust
/// Version 3: record that one fact replaced another.
///
/// `IF NOT EXISTS` so the statements are a no-op on a database that got the
/// columns from `SCHEMA` directly. Existing rows migrate to NULL/NULL and stay
/// live.
const MIGRATE_V3: &[&str] = &[
    "ALTER TABLE memories ADD COLUMN IF NOT EXISTS superseded_by UUID",
    "ALTER TABLE memories ADD COLUMN IF NOT EXISTS invalid_at TIMESTAMPTZ",
];
```

In `migrate`, after the existing `if found < 2 { ... }` block and before the `mem8_meta` rewrite:

```rust
    if found < 3 {
        for statement in MIGRATE_V3 {
            sqlx::query(statement)
                .execute(&mut *tx)
                .await
                .map_err(store_err)?;
        }
    }
```

This sits inside the same advisory lock and the same transaction that already serialize concurrent migrations, so the existing concurrent-migration test continues to cover it.

- [ ] **Step 4: Read, write, and filter the columns**

Add both fields to the row-mapping code (`superseded_by` maps to `Option<Uuid>`, `invalid_at` to `Option<DateTime<Utc>>` — both native sqlx types, so no parsing). Ensure every `SELECT` lists them.

Implement `supersede`:

```rust
    async fn supersede(&self, old: Uuid, new: Option<Uuid>, at: DateTime<Utc>) -> Result<()> {
        // `bind` on an Option writes NULL for None, so the optional successor
        // needs no special casing.
        let result = sqlx::query(
            "UPDATE memories SET superseded_by = $1, invalid_at = $2 WHERE id = $3",
        )
        .bind(new)
        .bind(at)
        .bind(old)
        .execute(&self.pool)
        .await
        .map_err(store_err)?;

        if result.rows_affected() == 0 {
            return Err(Mem8Error::NotFound(old.to_string()));
        }
        Ok(())
    }
```

Add the same three-mode predicate to `search` and `vector_search`, using `$n` placeholders and the file's existing conditional-clause pattern:

```rust
    match query.as_of {
        Some(_) => {
            clauses.push(format!("created_at <= ${n} AND (invalid_at IS NULL OR invalid_at > ${n})"));
        }
        None if !query.include_superseded => clauses.push("invalid_at IS NULL".into()),
        None => {}
    }
```

Bind the `as_of` timestamp once and reference it twice, or bind it twice — follow whichever the surrounding code makes natural. `vector_search` needs it too: a semantic search that ignored supersession would surface dead facts that keyword search correctly hides.

**Two divergence traps, both confirmed by review of Tasks 3-4:**

- **Use `TIMESTAMPTZ` for `invalid_at`, not TEXT.** SQLite stores it as RFC3339 text because that is what its other timestamps are; Postgres's `created_at`/`updated_at` are already `TIMESTAMPTZ`. Matching SQLite's TEXT here would give Postgres a text-comparison fragility it has no reason to carry. Both are temporally correct, so results still agree.
- **The predicate must go in the same SQL statement as the `LIMIT`.** Postgres applies `LIMIT` in SQL, so adding the temporal filter as a post-filter in Rust would truncate before filtering and diverge from both `MemStore` and SQLite, which filter before `.take(limit)`.

- [ ] **Step 5: Run tests to verify they pass**

```bash
MEM8_TEST_PG=1 cargo test --test pg_migration 2>&1 | tail -10
MEM8_TEST_PG=1 cargo test --test store_contract 2>&1 | tail -10
```
Expected: PASS both. The contract suite now proves SQLite and Postgres agree on supersession semantics.

- [ ] **Step 6: Flip the contract capability flag**

Task 5 added a `Supports { supersede: bool }` parameter to `run_contract` in `tests/store_contract.rs`, because the shared contract runs against both backends and Postgres could not yet supersede. Postgres's call site currently passes `supersede: false`.

**Flip it to `true`.** This is not optional bookkeeping — it is what makes the contract suite actually verify that SQLite and Postgres agree on supersession semantics, which is the entire reason the suite exists. Find it with:

```bash
grep -n "supersede: false" tests/store_contract.rs
```

Then run the contract suite against Postgres and confirm every supersession and `as_of` assertion now executes there. If any fails, the Postgres predicate disagrees with SQLite's — fix Postgres, not the assertion.

- [ ] **Step 7: Remove the interim stub deliberately**

Tasks 3-4 left `PgStore::supersede` returning `Mem8Error::Unsupported` so the tree would compile. Confirm you have replaced it with the real implementation and that no `Unsupported` stub remains:

```bash
grep -n "Unsupported" src/store/postgres.rs
```

Any hit inside `supersede` means the real implementation did not land. The contract suite in Task 5 already exercises `supersede` against whichever backend it runs, so a forgotten stub fails there rather than reaching a user — but check explicitly, because the stub errors rather than panics and is easy to miss.

- [ ] **Step 8: Commit**

```bash
git add src/store/postgres.rs tests/pg_migration.rs tests/store_contract.rs
git commit -m "feat(postgres): migrate to schema 3 with supersession columns"
```

---

## Task 7: Core write path

**Files:**
- Modify: `src/core.rs:322-363` (`add`), `src/core.rs:365-388` (`find_duplicate`)
- Test: `src/core.rs` (inline `mod tests`)

Two decisions are settled and must be implemented as stated:

**Validation runs before the embed call.** A rejected supersede must cost no model pass. This means resolving scope, then validating, then embedding — a change to `add`'s current line order, which embeds immediately after scope resolution.

**The new memory is written first, then the old one is invalidated.** The reverse order leaves a window where the old fact is dead and nothing has replaced it; a crash there loses the fact entirely. In the chosen order, a crash after the first step leaves two live memories — the condition that exists today, which the next write can still repair.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn supersedes_hides_the_old_memory_and_links_them() {
    let service = Memory8::new(Arc::new(MemStore::new()));
    let old = service
        .add("storage is sqlite", Kind::Decision, vec![], Some("p1".into()), None)
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

    // Six arguments here, not eight: Task 8 adds `include_superseded` and
    // `as_of` to this signature. When you reach that task, add `false, None`
    // to this call.
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
        .add("new fact", Kind::Decision, vec![], Some("p1".into()), Some(absent))
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
        .add("other project fact", Kind::Decision, vec![], Some("p2".into()), None)
        .await
        .unwrap();

    let err = service
        .add("new fact", Kind::Decision, vec![], Some("p1".into()), Some(other.id))
        .await
        .unwrap_err();

    // A cross-project supersession is far more likely a mistaken id than intent.
    assert!(matches!(err, Mem8Error::InvalidInput(_)));
    let message = err.to_string();
    assert!(message.contains("p1") && message.contains("p2"), "got: {message}");
}

#[tokio::test]
async fn superseding_an_already_superseded_memory_is_rejected() {
    let service = Memory8::new(Arc::new(MemStore::new()));
    let first = service
        .add("v1", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();
    let second = service
        .add("v2 entirely different words", Kind::Decision, vec![], Some("p1".into()), Some(first.id))
        .await
        .unwrap();

    // Chains stay linear: a memory has at most one successor.
    let err = service
        .add("v3 other words again", Kind::Decision, vec![], Some("p1".into()), Some(first.id))
        .await
        .unwrap_err();
    assert!(matches!(err, Mem8Error::InvalidInput(_)));
    assert!(
        err.to_string().contains(&second.id.to_string()),
        "error must name the existing successor, got: {err}"
    );
}

#[tokio::test]
async fn explicit_supersedes_skips_duplicate_detection() {
    let service = Memory8::new(Arc::new(MemStore::new()));
    let old = service
        .add("we use the porter tokenizer", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();

    // Near-identical content would normally be revised in place. With an
    // explicit supersedes the agent has already said what this replaces, so
    // re-deriving it by word overlap can only disagree with a direct answer.
    let new = service
        .add("we use the porter tokenizer", Kind::Decision, vec![], Some("p1".into()), Some(old.id))
        .await
        .unwrap();

    assert_ne!(new.id, old.id, "must create a new memory, not revise the old one");
    assert_eq!(service.all().await.unwrap().len(), 2);
}

#[tokio::test]
async fn duplicate_detection_ignores_superseded_memories() {
    let service = Memory8::new(Arc::new(MemStore::new()));
    let old = service
        .add("we use the porter tokenizer", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();
    let replacement = service
        .add("something else entirely", Kind::Decision, vec![], Some("p1".into()), Some(old.id))
        .await
        .unwrap();

    // Re-saving the old wording must not merge into the dead memory.
    let fresh = service
        .add("we use the porter tokenizer", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();

    assert_ne!(fresh.id, old.id, "a new write must never merge into a dead memory");
    assert_ne!(fresh.id, replacement.id);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib core::tests 2>&1 | tail -20`
Expected: FAIL to compile — `add` takes 4 arguments, not 5.

- [ ] **Step 3: Change the `add` signature and add validation**

Replace `Core::add` (or `Memory8::add` — use the actual type name in the file) with:

```rust
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

            if let Some(existing) = target.superseded_by {
                return Err(Mem8Error::InvalidInput(format!(
                    "memory {old_id} is already superseded by {existing}; \
                     supersede that one instead so the chain stays linear"
                )));
            }
        }

        let embedding = self.try_embed(&content);

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
```

Add `use chrono::Utc;` to `src/core.rs` if absent.

- [ ] **Step 4: Leave `get` alone — deliberately**

Do not add filtering to `Core::get` or to either backend's `get`. A superseded
memory must stay retrievable in full by id: it is hidden from discovery, not
deleted, and that distinction is the entire reason this feature is not
`delete_memory`. The tests at Task 4 Step 2 and Task 5 Step 1 both assert it.

Add the rule as a comment so a later reader does not "fix" the inconsistency:

```rust
    /// Fetch a memory by id, superseded or not.
    ///
    /// Never filters on `invalid_at`. A replaced memory is hidden from search
    /// but still readable here, which is what keeps "we used SQLite until
    /// 2026-08-17" answerable.
    pub async fn get(&self, id: Uuid) -> Result<Memory> {
        self.store.get(id).await
    }
```

- [ ] **Step 5: Filter `find_duplicate`**

In `find_duplicate`, add the two fields to the `SearchQuery` it builds:

```rust
                limit: MAX_LIMIT,
                // Never merge a new write into a memory that is already dead.
                include_superseded: false,
                as_of: None,
```

- [ ] **Step 6: Update every `add` caller**

```bash
cargo build --all-targets 2>&1 | grep -E "^error" | head -20
```

Pass `None` at each existing call site — `src/mcp/mod.rs`, `src/cli/mod.rs` (import loop), and any tests. Task 9 changes the MCP one properly.

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib core::tests 2>&1 | tail -10`
Expected: PASS, all 6 new tests.

- [ ] **Step 8: Full suite green**

Run: `cargo test 2>&1 | grep -E "^test result"`
Expected: all `ok`.

- [ ] **Step 9: Commit**

```bash
git add -u
git commit -m "feat(core): add explicit supersedes to the write path"
```

---

## Task 8: Temporal queries (`as_of`)

**Files:**
- Modify: `src/core.rs:438-534` (`search`), `src/core.rs` (`vector_hits`)
- Test: `tests/store_contract.rs`, `src/core.rs` (inline)

`as_of` is a second predicate over the same columns — the store work is already done in Tasks 5 and 6. This task threads it through `core` and proves the boundary behaviour.

- [ ] **Step 1: Write the failing boundary test**

Add to `run_contract` in `tests/store_contract.rs`. Boundary behaviour is the whole point, so test before, exactly at, and after:

```rust
    // --- as_of ------------------------------------------------------------
    // Timestamps are explicit so the boundary is exact rather than racing
    // against wall-clock resolution.
    let t0 = chrono::Utc::now();
    let a = store
        .add(new_memory("p1", Kind::Fact, "temporal alpha", &[]))
        .await
        .unwrap();
    let b = store
        .add(new_memory("p1", Kind::Fact, "temporal beta", &[]))
        .await
        .unwrap();
    let invalidated_at = chrono::Utc::now() + chrono::Duration::seconds(10);
    store.supersede(a.id, Some(b.id), invalidated_at).await.unwrap();

    let as_of = |t: chrono::DateTime<chrono::Utc>| SearchQuery {
        as_of: Some(t),
        ..query("temporal")
    };

    // Before invalidation: the old fact was still believed.
    let before = store.search(as_of(invalidated_at - chrono::Duration::seconds(1))).await.unwrap();
    assert!(before.iter().any(|h| h.memory.id == a.id), "as_of before must include it");

    // Exactly at invalidation: it is no longer true. The predicate is
    // `invalid_at > T`, so T == invalid_at excludes it.
    let at = store.search(as_of(invalidated_at)).await.unwrap();
    assert!(!at.iter().any(|h| h.memory.id == a.id), "as_of at the boundary must exclude it");

    // After invalidation: excluded.
    let after = store.search(as_of(invalidated_at + chrono::Duration::seconds(1))).await.unwrap();
    assert!(!after.iter().any(|h| h.memory.id == a.id));

    // Before either memory existed: neither is returned.
    let ancient = store.search(as_of(t0 - chrono::Duration::days(1))).await.unwrap();
    assert!(!ancient.iter().any(|h| h.memory.id == a.id || h.memory.id == b.id));
```

- [ ] **Step 2: Run test to verify it fails or passes**

Run: `cargo test --test store_contract 2>&1 | tail -15`
Expected: PASS if Tasks 5 and 6 implemented the predicate correctly. If it fails at the boundary case, the predicate used `>=` where it must use `>` — fix the store, not the test. A memory whose `invalid_at` equals `T` was not true at `T`.

- [ ] **Step 3: Thread `as_of` through core::search**

Change the signature:

```rust
    pub async fn search(
        &self,
        query: &str,
        project: Option<String>,
        global: bool,
        kind: Option<Kind>,
        tags: Vec<String>,
        limit: Option<usize>,
        include_superseded: bool,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<Vec<SearchHit>> {
```

Pass both into the `SearchQuery` it builds, and add both parameters to `vector_hits` so they reach `VectorQuery` too. Every filter that applies to keyword search must apply to vector search, or a semantic hit resurrects a dead fact.

- [ ] **Step 4: Add a core-level test for the mutual exclusion**

The rejection lives at the tool boundary (Task 9), but `core` should not silently accept a contradiction either. Add:

```rust
#[tokio::test]
async fn as_of_with_include_superseded_is_rejected() {
    let service = Memory8::new(Arc::new(MemStore::new()));
    let err = service
        .search("x", Some("p1".into()), false, None, vec![], None, true, Some(Utc::now()))
        .await
        .unwrap_err();
    assert!(matches!(err, Mem8Error::InvalidInput(_)));
    let message = err.to_string();
    assert!(
        message.contains("as_of") && message.contains("include_superseded"),
        "error must name both parameters, got: {message}"
    );
}
```

Implement at the top of `search`:

```rust
        // `as_of` already specifies exactly which rows count, so combining it
        // with `include_superseded` is a contradiction rather than a
        // refinement. Reject rather than silently favouring one.
        if as_of.is_some() && include_superseded {
            return Err(Mem8Error::InvalidInput(
                "as_of and include_superseded cannot be combined: as_of already \
                 determines which memories were valid at that time"
                    .into(),
            ));
        }
```

- [ ] **Step 5: Update callers and run tests**

```bash
cargo build --all-targets 2>&1 | grep -E "^error" | head -20
```

Pass `false, None` at existing call sites. Then:

Run: `cargo test 2>&1 | grep -E "^test result"`
Expected: all `ok`.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(core): answer searches as of a past time"
```

---

## Task 9: MCP tool surface

**Files:**
- Modify: `src/mcp/mod.rs:12-27` (`AddMemoryParams`), `src/mcp/mod.rs:28-50` (`SearchMemoryParams`), `src/mcp/mod.rs:140` (`add_memory`), the search handler
- Test: `src/mcp/mod.rs` (inline) or `tests/e2e.rs`

The doc comments on these params are the agent-facing documentation — they are what the model reads to decide whether to pass `supersedes`. Write them for that audience, not for a human reading source.

- [ ] **Step 1: Write the failing test**

Add to `tests/e2e.rs` (or the inline MCP tests, matching where existing tool-surface tests live):

```rust
#[tokio::test]
async fn add_memory_accepts_supersedes_and_hides_the_old_fact() {
    // Use whatever harness the existing e2e tests use to build a service over
    // a temp store; mirror the closest existing test's setup exactly.
    let service = test_service().await;

    let old = service
        .add("storage is sqlite", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();
    service
        .add("storage is postgres", Kind::Decision, vec![], Some("p1".into()), Some(old.id))
        .await
        .unwrap();

    let hits = service
        .search("storage", Some("p1".into()), false, None, vec![], None, false, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "only the live fact should be returned");
    assert_eq!(hits[0].memory.content, "storage is postgres");
}

#[tokio::test]
async fn search_rejects_as_of_with_include_superseded() {
    let service = test_service().await;
    let err = service
        .search("x", Some("p1".into()), false, None, vec![], None, true, Some(chrono::Utc::now()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("as_of"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test e2e 2>&1 | tail -20`
Expected: FAIL to compile if the harness helper does not exist under that name — adapt to the file's actual helper before proceeding.

- [ ] **Step 3: Add the parameters**

To `AddMemoryParams`:

```rust
    /// The id of a memory this one replaces, when the project has changed its
    /// mind.
    ///
    /// Pass this when the new memory contradicts an existing one you just found
    /// by searching — the old fact stops being returned by search but stays
    /// retrievable by id, so past decisions remain explicable. Leave it unset
    /// when the memory is simply new information.
    ///
    /// The target must be in the same project and must not already be
    /// superseded.
    pub supersedes: Option<Uuid>,
```

To `SearchMemoryParams`:

```rust
    /// Include memories that have been replaced by newer ones.
    ///
    /// Defaults to false, which returns only what is currently true. Set it to
    /// see the full history of a changed decision. Cannot be combined with
    /// `as_of`.
    pub include_superseded: Option<bool>,
    /// Answer as of a past instant, RFC3339 — what was believed then.
    ///
    /// Returns memories created before this time that had not yet been replaced
    /// by it. Cannot be combined with `include_superseded`.
    pub as_of: Option<DateTime<Utc>>,
```

`Uuid` and `DateTime<Utc>` both need to satisfy `schemars::JsonSchema` for the derive to work. `uuid` needs its `schemars` support and `chrono` likewise — check `Cargo.toml`. If a feature is missing, either enable it, or take these as `String` at the boundary and parse with a named error:

```rust
    let as_of = match params.as_of.as_deref() {
        None => None,
        Some(raw) => Some(
            DateTime::parse_from_rfc3339(raw)
                .map_err(|e| Mem8Error::InvalidInput(format!(
                    "as_of must be an RFC3339 timestamp like 2026-08-17T00:00:00Z: {e}"
                )))?
                .with_timezone(&Utc),
        ),
    };
```

Prefer the typed version if the features are already on; the schema an agent reads is clearer for it.

- [ ] **Step 4: Wire the handlers**

In `add_memory`, pass `params.supersedes` through as the new fifth argument. In the search handler, pass `params.include_superseded.unwrap_or(false)` and the parsed `as_of`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test 2>&1 | grep -E "^test result"`
Expected: all `ok`.

- [ ] **Step 6: Verify the generated schema**

The MCP schema is the agent's only documentation, and `Option<Uuid>` can serialize in surprising ways. Confirm the tool definitions still look right:

```bash
cargo test --test e2e 2>&1 | tail -5
```

If the repo has a schema-snapshot test, update the snapshot and read the diff — check that `supersedes` appears as a nullable string with a uuid format, and that `kind` is still one flat `enum` rather than a `oneOf` (the `NewMemory::Default` comment in `src/model.rs` explains why that matters).

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "feat(mcp): expose supersedes, include_superseded, and as_of"
```

---

## Task 10: Markdown export and import

**Files:**
- Modify: `src/cli/markdown.rs:20-34` (`to_markdown`), `src/cli/markdown.rs:54-140` (`from_markdown`)
- Test: `src/cli/markdown.rs` (inline `mod tests`)

**The signature must change.** `from_markdown` returns `Vec<NewMemory>`, and `NewMemory` has no id. Remapping `superseded_by` needs each section's original heading uuid to build the old→new mapping, so the return type must carry it. This is the part the spec glosses over.

Without this round-tripping, `mem8 export` followed by `mem8 import` silently resurrects every dead fact, and the backup path becomes the way contradictions come back.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn live_memory_exports_byte_identically_to_before() {
    let m = memory("Plain live fact.", vec![]);
    let text = to_markdown(&[m]);
    assert!(!text.contains("superseded_by"), "got: {text}");
    assert!(!text.contains("invalid_at"), "got: {text}");
}

#[test]
fn superseded_memory_roundtrips_with_remapped_successor() {
    let now = Utc::now();
    let old_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();

    let mut old = memory("storage is sqlite", vec![]);
    old.id = old_id;
    old.superseded_by = Some(new_id);
    old.invalid_at = Some(now);

    let mut new = memory("storage is postgres", vec![]);
    new.id = new_id;

    let text = to_markdown(&[old, new]);
    assert!(text.contains(&new_id.to_string()), "successor uuid must be written");

    let parsed = from_markdown(&text).unwrap();
    assert_eq!(parsed.len(), 2);

    // The parsed form carries the original heading id, so import can remap.
    assert_eq!(parsed[0].original_id, Some(old_id));
    assert_eq!(parsed[0].superseded_by, Some(new_id));
    assert!(parsed[0].invalid_at.is_some());
    assert_eq!(parsed[1].superseded_by, None);
}

#[test]
fn a_file_without_the_new_fields_still_imports() {
    // Existing export files must keep loading: both fields are optional.
    let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags: []\n- created: 2026-08-11T00:00:00+00:00\n\nBody.\n";
    let parsed = from_markdown(text).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].superseded_by, None);
    assert_eq!(parsed[0].invalid_at, None);
}

#[test]
fn an_unparseable_invalid_at_is_an_error_naming_the_section() {
    let text = "## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n- project: p\n- kind: decision\n- tags: []\n- created: 2026-08-11T00:00:00+00:00\n- invalid_at: not-a-date\n\nBody.\n";
    let err = from_markdown(text).unwrap_err().to_string();
    assert!(err.contains("7a1f7a1f"), "error should identify the section, got: {err}");
    assert!(err.contains("invalid_at"), "got: {err}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::markdown 2>&1 | tail -20`
Expected: FAIL to compile — no field `original_id` on the parsed type.

- [ ] **Step 3: Add the parsed-section type**

In `src/cli/markdown.rs`:

```rust
/// A memory parsed from a markdown file, with the fields import needs that
/// `NewMemory` cannot carry.
///
/// `NewMemory` has no id, deliberately — import always creates fresh rows so it
/// cannot collide with existing identifiers. But remapping `superseded_by`
/// needs the file's own ids to build the old-to-new mapping, so the parsed form
/// keeps them alongside.
#[derive(Debug, Clone)]
pub struct ParsedMemory {
    pub new: NewMemory,
    /// The uuid from the section heading, used only to resolve `superseded_by`
    /// against other sections in the same file.
    pub original_id: Option<Uuid>,
    /// The successor's *original* id, to be remapped on import.
    pub superseded_by: Option<Uuid>,
    pub invalid_at: Option<DateTime<Utc>>,
}
```

The tests above access `parsed[0].superseded_by` directly, so keep these as fields on `ParsedMemory` rather than nesting them.

- [ ] **Step 4: Write the fields on export**

In `to_markdown`, after the `created` line, append the two optional lines only when set, so a live memory exports byte-identically to today:

```rust
        let mut extra = String::new();
        if let Some(successor) = m.superseded_by {
            extra.push_str(&format!("- superseded_by: {successor}\n"));
        }
        if let Some(invalid) = m.invalid_at {
            extra.push_str(&format!("- invalid_at: {}\n", invalid.to_rfc3339()));
        }
```

Insert `{extra}` into the format string immediately after the `created` line and before the blank line that starts the body. Keep the blank-line-before-body invariant exactly — `from_markdown` uses it to detect the body boundary, and an extra or missing blank line breaks every existing round-trip test.

- [ ] **Step 5: Parse the fields**

Change the signature to `pub fn from_markdown(text: &str) -> Result<Vec<ParsedMemory>>`.

Add two branches to the header-parsing loop, alongside the existing `- created:` branch:

```rust
            } else if let Some(v) = line.strip_prefix("- superseded_by:") {
                superseded_by = Some(Uuid::parse_str(v.trim()).map_err(|e| {
                    Mem8Error::InvalidInput(format!(
                        "section '{heading_id}' has an unparseable 'superseded_by': {e}"
                    ))
                })?);
            } else if let Some(v) = line.strip_prefix("- invalid_at:") {
                invalid_at = Some(
                    DateTime::parse_from_rfc3339(v.trim())
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(|e| {
                            Mem8Error::InvalidInput(format!(
                                "section '{heading_id}' has an unparseable 'invalid_at': {e}"
                            ))
                        })?,
                );
```

Declare `let mut superseded_by = None;` and `let mut invalid_at = None;` beside the existing locals, and build `ParsedMemory` at the end with `original_id: Uuid::parse_str(heading_id).ok()`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib cli::markdown 2>&1 | tail -10`
Expected: PASS. Existing round-trip tests must also still pass — they now index `.new.content` instead of `.content`, so update them mechanically.

- [ ] **Step 7: Commit**

```bash
git add src/cli/markdown.rs
git commit -m "feat(markdown): round-trip supersession fields"
```

---

## Task 11: Import remapping

**Files:**
- Modify: `src/cli/mod.rs:56-110` (`import`)
- Test: `tests/cli_roundtrip.rs`

Import creates new memories with new ids, so a `superseded_by` from the file points at an id that no longer exists. Load first, then rewrite the pointers using the mapping built during load.

- [ ] **Step 1: Write the failing test**

Add to `tests/cli_roundtrip.rs`, following its existing pattern for setting `MEM8_DB` to a temp database:

```rust
#[tokio::test]
async fn superseded_memory_survives_an_export_import_round_trip() {
    // This test is the reason the fields are round-tripped at all: without it,
    // export-then-import silently resurrects every dead fact and the backup
    // path becomes how contradictions come back.
    let dir = std::env::temp_dir().join(format!("mem8-rt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("source.db");
    std::env::set_var("MEM8_DB", format!("sqlite://{}", db.display()));

    let service = mem8::Memory8::new(mem8::store::open_from_env().await.unwrap());
    let old = service
        .add("storage is sqlite", Kind::Decision, vec![], Some("p1".into()), None)
        .await
        .unwrap();
    let new = service
        .add("storage is postgres", Kind::Decision, vec![], Some("p1".into()), Some(old.id))
        .await
        .unwrap();
    assert_eq!(service.get(old.id).await.unwrap().superseded_by, Some(new.id));

    let file = dir.join("export.md");
    export(&file).await.unwrap();

    // Import into a fresh database.
    let target = dir.join("target.db");
    std::env::set_var("MEM8_DB", format!("sqlite://{}", target.display()));
    let count = import(&file).await.unwrap();
    assert_eq!(count, 2);

    let imported = mem8::Memory8::new(mem8::store::open_from_env().await.unwrap());

    // The dead fact came back dead.
    let all = imported.all().await.unwrap();
    let dead: Vec<_> = all.iter().filter(|m| m.invalid_at.is_some()).collect();
    assert_eq!(dead.len(), 1, "exactly one memory must import as superseded");
    assert_eq!(dead[0].content, "storage is sqlite");

    // Its successor pointer was remapped to the newly imported id, not the old one.
    let successor = dead[0].superseded_by.expect("successor must be set");
    assert_ne!(successor, new.id, "pointer must be remapped, not copied verbatim");
    assert_eq!(
        imported.get(successor).await.unwrap().content,
        "storage is postgres",
        "remapped pointer must resolve to the right memory"
    );

    // Search returns only the live fact.
    let hits = imported
        .search("storage", Some("p1".into()), false, None, vec![], None, false, None)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}
```

Check how the existing tests in this file handle `MEM8_DB` — if they use a guard or run single-threaded, follow that. Env vars are process-global and these tests will interfere if run in parallel.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test cli_roundtrip 2>&1 | tail -20`
Expected: FAIL — nothing imports as superseded, because `import` drops the fields.

- [ ] **Step 3: Implement the remapping pass**

Rewrite the body of `import` after parsing:

```rust
    let store = open_from_env().await?;
    let service = Arc::new(Memory8::new(store.clone()));

    // Two passes. Load every memory first, recording old id -> new id, then
    // rewrite the successor pointers. A one-pass version cannot work: a
    // memory's successor may appear later in the file than it does.
    let mut mapping: std::collections::HashMap<Uuid, Uuid> = std::collections::HashMap::new();
    let mut pending: Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> = Vec::new();
    let mut count = 0;

    for parsed in incoming {
        let created = store.add(parsed.new).await?;
        count += 1;

        if let Some(original) = parsed.original_id {
            mapping.insert(original, created.id);
        }
        if let Some(invalid_at) = parsed.invalid_at {
            pending.push((created.id, parsed.superseded_by, invalid_at));
        }
    }

    for (new_id, old_successor, invalid_at) in pending {
        match old_successor.and_then(|s| mapping.get(&s).copied()) {
            Some(successor) => store.supersede(new_id, successor, invalid_at).await?,
            None => {
                // A successor outside this file: the memory is still known to
                // be dead, only its successor is unknown. This is exactly why
                // `supersede` takes `Option` — dropping the invalidation
                // instead would resurrect the fact, which is the failure this
                // round-tripping exists to prevent.
                if old_successor.is_some() {
                    eprintln!(
                        "warning: imported memory {new_id} was superseded by a memory not \
                         present in this file; keeping it invalid with no successor recorded"
                    );
                }
                store.supersede(new_id, None, invalid_at).await?;
            }
        }
    }
```

Note the `if old_successor.is_some()` guard on the warning: a file may legitimately carry `invalid_at` with no `superseded_by` at all (that is what this branch writes on export), and warning about a pointer that was never there would be noise.

- [ ] **Step 4: Add a test for the unresolvable-successor case**

This is the branch the spec calls out explicitly, so it needs its own test. Add to `tests/cli_roundtrip.rs`:

```rust
#[tokio::test]
async fn a_successor_missing_from_the_file_leaves_the_memory_dead() {
    let dir = std::env::temp_dir().join(format!("mem8-orphan-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("target.db");
    std::env::set_var("MEM8_DB", format!("sqlite://{}", db.display()));

    // A hand-written file whose successor uuid appears in no section.
    let file = dir.join("orphan.md");
    std::fs::write(
        &file,
        "# mem8 export\n\n\
         ## 7a1f7a1f-7a1f-7a1f-7a1f-7a1f7a1f7a1f\n\
         - project: p1\n- kind: decision\n- tags: []\n\
         - created: 2026-08-01T00:00:00+00:00\n\
         - superseded_by: 9b2e9b2e-9b2e-9b2e-9b2e-9b2e9b2e9b2e\n\
         - invalid_at: 2026-08-17T00:00:00+00:00\n\n\
         storage is sqlite\n\n",
    )
    .unwrap();

    assert_eq!(import(&file).await.unwrap(), 1);

    let service = mem8::Memory8::new(mem8::store::open_from_env().await.unwrap());
    let all = service.all().await.unwrap();
    assert_eq!(all.len(), 1);

    // Still known to be dead; only the successor is unknown.
    assert!(all[0].invalid_at.is_some(), "the fact must not be resurrected");
    assert_eq!(all[0].superseded_by, None, "an unresolvable pointer is dropped");

    // And it stays hidden from search.
    let hits = service
        .search("storage", Some("p1".into()), false, None, vec![], None, false, None)
        .await
        .unwrap();
    assert!(hits.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test cli_roundtrip 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 6: Full suite green**

Run: `cargo test 2>&1 | grep -E "^test result"`
Expected: all `ok`.

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "feat(cli): remap successor pointers on import"
```

---

## Task 12: README

**Files:**
- Modify: `README.md`

The change to `search_memory`'s default result set is a behaviour change to the tool contract even though no existing memory is affected, so it belongs in the README's own words rather than only in a commit message.

- [ ] **Step 1: Update the tool table**

Add `supersedes` to `add_memory`'s parameters, and `include_superseded` / `as_of` to `search_memory`'s. Match the table's existing column layout exactly.

- [ ] **Step 2: Add a supersession section**

Place it after the section describing search. Write it as prose the user reads to decide whether to use the feature:

```markdown
## When a decision changes

A project that changes its mind should not return both answers. Pass
`supersedes` with the id of the memory being replaced:

    add_memory(content: "Default backend is Postgres", kind: "decision",
               supersedes: "<id of the SQLite decision>")

The old memory stops appearing in search but stays retrievable by id, so
"we used SQLite until 2026-08-17" is still answerable. That is the difference
between superseding a memory and deleting it.

Two ways to see past state:

- `include_superseded: true` returns replaced memories alongside live ones.
- `as_of: "2026-08-01T00:00:00Z"` returns what was believed at that time.

The two cannot be combined — `as_of` already determines which memories count.

**Search now returns only live memories by default.** Nothing is deleted on
upgrade, and every memory that exists today stays live; but a search that
previously returned a contradicted fact will stop returning it once something
supersedes it.
```

- [ ] **Step 3: Document the markdown fields**

Add both optional lines to the export-format description, noting they appear only on superseded memories:

```
- superseded_by: <uuid>
- invalid_at: <rfc3339>
```

Note that import remaps `superseded_by` to the newly created ids, and that a successor missing from the file leaves the memory invalid with no successor recorded.

- [ ] **Step 4: Verify no stale claims remain**

```bash
grep -n "every memory\|all memories\|returns every" README.md
```

Read each hit and check it is still true now that search filters by default.

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: document supersession, as_of, and the new format fields"
```

---

## Final verification

- [ ] **Full suite, both backends**

```bash
cargo test 2>&1 | grep -E "^test result"
MEM8_TEST_PG=1 cargo test 2>&1 | grep -E "^test result"
```

Every line must read `ok`. If Postgres is unavailable, say so explicitly rather than reporting a pass — the contract suite's entire purpose is proving both backends agree.

- [ ] **Clippy and formatting**

```bash
cargo clippy --all-targets 2>&1 | grep -E "^(warning|error)" | head -20
cargo fmt --check
```

- [ ] **Feature combinations still build**

Supersession touches `vector_search`, which is behind a feature flag:

```bash
cargo build --features semantic 2>&1 | tail -5
cargo build --features http 2>&1 | tail -5
cargo build --all-features 2>&1 | tail -5
```

- [ ] **Upgrade an existing database by hand**

The migration is the one irreversible part of this change. Prove it on a real database before trusting it:

```bash
cp ~/.mem8/mem8.db /tmp/mem8-backup.db
cargo run -- search "anything" 2>&1 | tail -5
```

Expected: the search works, and previously stored memories are still returned. If anything is wrong, restore from `/tmp/mem8-backup.db`.

- [ ] **Verify the spec is fully covered**

Re-read `docs/superpowers/specs/2026-08-17-fact-supersession-design.md` sections 1–4, 6, and 7 and confirm each requirement maps to a task above. Sections 5 (semantic dedup) and the recency-decay half of the spec are deliberately out of scope for this plan.
