use mem8::core::sanitize_fts_query;
use mem8::model::{Kind, MemoryUpdate, NewMemory, SearchQuery};
use mem8::store::sqlite::SqliteStore;
use mem8::store::Store;

fn new_memory(project: &str, kind: Kind, content: &str, tags: &[&str]) -> NewMemory {
    NewMemory {
        project: project.into(),
        kind,
        content: content.into(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn query(text: &str) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        project: Some("p1".into()),
        global: false,
        kind: None,
        tags: vec![],
        limit: 10,
        include_superseded: false,
        as_of: None,
    }
}

/// What a backend can do, so the shared contract can assert the right subset
/// without probing for capabilities by attempting writes.
///
/// A backend that has not yet grown a feature passes `false` and the contract
/// skips exactly that block -- but the caller has to say so out loud at the
/// call site, so a missing implementation is visible in the source rather
/// than hidden behind a swallowed stderr line on a green run.
pub struct Supports {
    pub supersede: bool,
}

/// The behaviour every backend must share. Run once per implementation so the
/// SQLite and Postgres stores cannot silently diverge.
pub async fn run_contract(store: &dyn Store, supports: Supports) {
    // Round-trip preserves every field.
    let added = store
        .add(new_memory("p1", Kind::Decision, "we chose rust", &["lang"]))
        .await
        .unwrap();
    let got = store.get(added.id).await.unwrap();
    assert_eq!(got.content, "we chose rust");
    assert_eq!(got.kind, Kind::Decision);
    assert_eq!(got.tags, vec!["lang".to_string()]);
    assert!(got.embedding.is_none(), "embedding must be NULL in v1");

    // Timestamps are set and ordered sanely.
    assert_eq!(got.created_at, added.created_at);
    assert!(got.updated_at >= got.created_at);

    // Search finds it, scoped to its project.
    let hits = store.search(query("rust")).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory.id, added.id);

    // Scope isolation: another project's memory is not returned.
    store
        .add(new_memory("p2", Kind::Decision, "we chose rust", &[]))
        .await
        .unwrap();
    assert_eq!(store.search(query("rust")).await.unwrap().len(), 1);

    // Global search crosses projects.
    let global = SearchQuery { global: true, project: None, ..query("rust") };
    assert_eq!(store.search(global).await.unwrap().len(), 2);

    // Kind filter.
    store
        .add(new_memory("p1", Kind::Convention, "rust files use snake_case", &[]))
        .await
        .unwrap();
    let by_kind = SearchQuery { kind: Some(Kind::Convention), ..query("rust") };
    assert_eq!(store.search(by_kind).await.unwrap().len(), 1);

    // Tag filter uses AND semantics.
    store
        .add(new_memory("p1", Kind::Fact, "rust tooling notes", &["lang", "tools"]))
        .await
        .unwrap();
    let both = SearchQuery { tags: vec!["lang".into(), "tools".into()], ..query("rust") };
    assert_eq!(store.search(both).await.unwrap().len(), 1);

    // Limit is honoured.
    let limited = SearchQuery { limit: 1, ..query("rust") };
    assert_eq!(store.search(limited).await.unwrap().len(), 1);

    // Update changes content and bumps updated_at.
    let updated = store
        .update(added.id, MemoryUpdate { content: Some("we chose go".into()), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(updated.content, "we chose go");
    assert!(updated.updated_at >= updated.created_at);

    // The search index reflects the update.
    assert!(store
        .search(query("rust"))
        .await
        .unwrap()
        .iter()
        .all(|h| h.memory.id != added.id));

    // Missing identifiers are errors, not silent successes.
    let ghost = uuid::Uuid::new_v4();
    assert!(store.get(ghost).await.is_err());
    assert!(store.delete(ghost).await.is_err());
    assert!(store.update(ghost, MemoryUpdate::default()).await.is_err());

    // Delete removes the row and its index entry.
    store.delete(added.id).await.unwrap();
    assert!(store.get(added.id).await.is_err());

    // `all` returns every remaining memory in creation order.
    let all = store.all().await.unwrap();
    assert!(all.windows(2).all(|w| w[0].created_at <= w[1].created_at));

    // Hyphenated identifiers are findable as a unit, on both backends. The
    // `Store` trait receives text already sanitized by
    // `core::sanitize_fts_query` (that is `Memory8::search`'s job, not the
    // store's) -- passing a raw hyphenated string straight to FTS5 MATCH
    // errors with "no such column", so this test goes through the real
    // sanitizer, exactly as production code does. Naive punctuation-stripping
    // would have split "auth-token" into two bare terms, letting a query
    // match unrelated documents containing "auth" and "token" far apart;
    // quoting the term as a literal phrase avoids that.
    let hyphen_id = store
        .add(new_memory("p3", Kind::Fact, "we use auth-token for login", &[]))
        .await
        .unwrap();
    let hyphen_text = sanitize_fts_query("auth-token").unwrap();
    let hyphen_query = SearchQuery { project: Some("p3".into()), ..query(&hyphen_text) };
    let hyphen_hits = store.search(hyphen_query).await.unwrap();
    assert_eq!(hyphen_hits.len(), 1);
    assert_eq!(hyphen_hits[0].memory.id, hyphen_id.id);

    // Stemming: SQLite's FTS5 table now uses the porter tokenizer, matching
    // Postgres's `to_tsvector('english', ...)`, so the two backends must
    // agree on stem variants. "running" is unambiguous in project "p4" (no
    // other memory there contains any form of "run"), so a search for the
    // stem "run" finding exactly it proves the backends now agree rather
    // than the query happening to match on a bare substring.
    let stem_id = store
        .add(new_memory("p4", Kind::Fact, "the team is running tests", &[]))
        .await
        .unwrap();
    let stem_text = sanitize_fts_query("run").unwrap();
    let stem_query = SearchQuery { project: Some("p4".into()), ..query(&stem_text) };
    let stem_hits = store.search(stem_query).await.unwrap();
    assert_eq!(stem_hits.len(), 1);
    assert_eq!(stem_hits[0].memory.id, stem_id.id);

    // --- Supersession ---------------------------------------------------
    //
    // Gated on an explicit capability flag rather than a probe write: Task 6
    // still has Postgres's `supersede` returning `Unsupported`, and flipping
    // `supports.supersede` to `true` at that call site is a required step of
    // that task (grep for `supersede: false` below). Using a block rather
    // than an early `return` means anything appended after this section
    // keeps running on every backend, including ones that skip this block.
    if supports.supersede {
        let old = store
            .add(new_memory("p1", Kind::Decision, "storage is sqlite", &[]))
            .await
            .unwrap();
        let replacement = store
            .add(new_memory("p1", Kind::Decision, "storage is postgres", &[]))
            .await
            .unwrap();
        // Explicit instants rather than wall-clock ones: `supersede` takes the
        // invalidation time from the caller, so the three boundary positions
        // can be exact on every platform instead of depending on how much
        // time two inserts happen to take. (A `created_at`-derived midpoint
        // is not safe here: at `at == old.created_at` the midpoint collapses
        // onto `created_at`, which satisfies `created_at <= t_before` but
        // fails `invalid_at > t_before`, since `t_before == at ==
        // invalid_at`. Fixed offsets from `old.created_at` sidestep that.)
        let at = old.created_at + chrono::Duration::seconds(10);
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
        assert!(both_ids.contains(&old.id), "include_superseded must still return the old fact");
        assert!(both_ids.contains(&replacement.id), "include_superseded must return the replacement too");

        // Superseding a memory that does not exist is NotFound.
        assert!(store
            .supersede(uuid::Uuid::new_v4(), Some(replacement.id), at)
            .await
            .is_err());

        // as_of: before, exactly at, and after the invalidation boundary.
        let t_before = old.created_at + chrono::Duration::seconds(5);
        let t_after = at + chrono::Duration::seconds(5);

        let as_of = |t| SearchQuery { as_of: Some(t), ..query("storage") };

        // Before invalidation the old fact was still believed.
        let before = store.search(as_of(t_before)).await.unwrap();
        assert!(
            before.iter().any(|h| h.memory.id == old.id),
            "as_of before the invalidation must include the old fact"
        );

        // Exactly at invalidation it is already dead: the predicate is
        // `invalid_at > T`, so T == invalid_at excludes it.
        let at_boundary = store.search(as_of(at)).await.unwrap();
        assert!(
            !at_boundary.iter().any(|h| h.memory.id == old.id),
            "as_of exactly at the invalidation must exclude it"
        );

        // After invalidation, excluded.
        let after = store.search(as_of(t_after)).await.unwrap();
        assert!(!after.iter().any(|h| h.memory.id == old.id));
    } else {
        // If a backend grows a real `supersede`, this contract must start
        // exercising it. Asserting the stub is still a stub turns a forgotten
        // flag flip into a failing test that names the fix, rather than a
        // silently reduced contract on a green run.
        let probe = store
            .supersede(uuid::Uuid::new_v4(), None, chrono::Utc::now())
            .await;
        assert!(
            matches!(probe, Err(mem8::error::Mem8Error::Unsupported { .. })),
            "supersede is implemented but the contract is still gated off — \
             flip supports.supersede at this backend's call site"
        );
    }
}

#[tokio::test]
async fn sqlite_satisfies_the_store_contract() {
    let store = SqliteStore::open_in_memory().unwrap();
    run_contract(&store, Supports { supersede: true }).await;
}

/// `SqliteStore::open` (the file-backed constructor) was previously exercised
/// by no test — only `open_in_memory` was covered. It also does
/// `create_dir_all` on the parent directory, which needs a path with a
/// missing parent to actually exercise. This test closes that gap.
#[tokio::test]
async fn sqlite_open_creates_the_database_file() {
    let dir = std::env::temp_dir()
        .join("mem8-open-test")
        .join(uuid::Uuid::new_v4().to_string())
        .join("nested");
    let db_path = dir.join("mem8.db");

    // `dir` (and its parent `mem8-open-test/<uuid>`) do not exist yet, so
    // `SqliteStore::open` must create them via `create_dir_all`.
    assert!(!dir.exists());

    let store = SqliteStore::open(&db_path).unwrap();
    let added = store
        .add(new_memory("p1", Kind::Decision, "file backed store works", &[]))
        .await
        .unwrap();
    let got = store.get(added.id).await.unwrap();
    assert_eq!(got.content, "file backed store works");

    assert!(db_path.exists(), "database file should exist on disk after open()");

    // Drop the store so the file handle is released before cleanup on
    // platforms that lock open files.
    drop(store);

    // Clean up: remove the whole uuid-named directory tree we created.
    let cleanup_root = dir.parent().unwrap(); // mem8-open-test/<uuid>
    std::fs::remove_dir_all(cleanup_root).unwrap();
    assert!(!cleanup_root.exists());
}

/// Postgres is opt-in. Set `MEM8_TEST_PG` to a connection string to run this;
/// a plain `cargo test` must pass with no database server running.
#[tokio::test]
async fn postgres_satisfies_the_store_contract() {
    let Ok(url) = std::env::var("MEM8_TEST_PG") else {
        eprintln!("skipping: MEM8_TEST_PG not set");
        return;
    };

    let store = mem8::store::postgres::PgStore::connect(&url).await.unwrap();
    store.reset_for_tests().await.unwrap();
    // Task 6 flips this to `true` once Postgres grows real supersession
    // columns and a working `supersede`; until then it stays `Unsupported`.
    run_contract(&store, Supports { supersede: false }).await;
}
