use mem8::model::{Kind, MemoryUpdate, NewMemory, SearchQuery};
use mem8::store::sqlite::SqliteStore;
use mem8::store::Store;

fn new_memory(project: &str, kind: Kind, content: &str, tags: &[&str]) -> NewMemory {
    NewMemory {
        project: project.into(),
        kind,
        content: content.into(),
        tags: tags.iter().map(|s| s.to_string()).collect(),
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
    }
}

/// The behaviour every backend must share. Run once per implementation so the
/// SQLite and Postgres stores cannot silently diverge.
pub async fn run_contract(store: &dyn Store) {
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
}

#[tokio::test]
async fn sqlite_satisfies_the_store_contract() {
    let store = SqliteStore::open_in_memory().unwrap();
    run_contract(&store).await;
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
