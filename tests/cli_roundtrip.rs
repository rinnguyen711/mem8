use mem8::core::{Memory8, SearchOptions};
use mem8::model::Kind;

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mem8-rt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Every test here points `MEM8_DB` at its own temporary database, and that
/// variable is process-global: two of these running at once would each see the
/// other's setting. This mutex is what keeps them from doing so. Hold it for the
/// whole body of any test that touches `MEM8_DB` -- releasing it early puts the
/// environment back in play while the test is still using it.
///
/// A `tokio` mutex rather than `std`'s, because the guard is held across awaits.
/// It also has no poisoning, so one failing test does not turn every later one
/// into a second failure that hides it.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `export` followed by `import` must reproduce the same memory set. This is
/// what keeps the markdown format honest.
#[tokio::test]
async fn export_then_import_reproduces_every_memory() {
    let _env = ENV.lock().await;
    let source_db = temp_path("source.db");
    let target_db = temp_path("target.db");
    let markdown = temp_path("memories.md");

    // Populate the source database.
    std::env::set_var("MEM8_DB", format!("sqlite://{}", source_db.display()));
    {
        let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
        service
            .add(
                "We chose Rust.",
                Kind::Decision,
                vec!["lang".into()],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        service
            .add(
                "Tests use cargo test.",
                Kind::Convention,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        service
            .add(
                "Multi\n\nparagraph body.",
                Kind::Fact,
                vec!["a".into(), "b".into()],
                Some("p2".into()),
                None,
            )
            .await
            .unwrap();
    }

    let exported = mem8::cli::export(&markdown).await.unwrap();
    assert_eq!(exported, 3);

    // Import into a fresh database.
    std::env::set_var("MEM8_DB", format!("sqlite://{}", target_db.display()));
    let imported = mem8::cli::import(&markdown).await.unwrap();
    assert_eq!(imported, 3);

    let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
    let all = service.all().await.unwrap();
    assert_eq!(all.len(), 3);

    let contents: Vec<&str> = all.iter().map(|m| m.content.as_str()).collect();
    assert!(contents.contains(&"We chose Rust."));
    assert!(contents.contains(&"Multi\n\nparagraph body."));

    let rust_memory = all.iter().find(|m| m.content == "We chose Rust.").unwrap();
    assert_eq!(rust_memory.kind, Kind::Decision);
    assert_eq!(rust_memory.project, "p1");
    assert_eq!(rust_memory.tags, vec!["lang".to_string()]);

    let multi = all.iter().find(|m| m.project == "p2").unwrap();
    assert_eq!(multi.tags, vec!["a".to_string(), "b".to_string()]);

    // Clean up the temp files/directories this test created. Drop `service`
    // (and its underlying SQLite connection) first: on Windows the database
    // file cannot be removed while a connection is still open.
    drop(service);
    for path in [&source_db, &target_db, &markdown] {
        if let Some(dir) = path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

/// A superseded memory must survive the round trip as superseded. This test is
/// the reason the supersession fields are written to the file at all: without
/// it, `export` followed by `import` silently resurrects every dead fact, and
/// the backup path becomes the way contradictions come back.
///
/// The successor's id in the file is the *source* database's id, which does not
/// exist in the target -- import creates fresh rows. So the assertion that
/// matters is not that the pointer survived but that it was remapped onto the
/// newly created successor.
#[tokio::test]
async fn superseded_memory_survives_an_export_import_round_trip() {
    let _env = ENV.lock().await;
    let source_db = temp_path("source.db");
    let target_db = temp_path("target.db");
    let markdown = temp_path("memories.md");

    std::env::set_var("MEM8_DB", format!("sqlite://{}", source_db.display()));
    let (old_id, new_id) = {
        let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
        let old = service
            .add(
                "The database is SQLite.",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                None,
            )
            .await
            .unwrap();
        let new = service
            .add(
                "The database is Postgres.",
                Kind::Fact,
                vec![],
                Some("p1".into()),
                Some(old.id),
            )
            .await
            .unwrap();
        (old.id, new.id)
    };

    let exported = mem8::cli::export(&markdown).await.unwrap();
    assert_eq!(exported, 2);

    // The file carries the source ids, which the target database will not have.
    let text = std::fs::read_to_string(&markdown).unwrap();
    assert!(
        text.contains(&format!("- superseded_by: {new_id}")),
        "export must record the successor, got:\n{text}"
    );

    std::env::set_var("MEM8_DB", format!("sqlite://{}", target_db.display()));
    let imported = mem8::cli::import(&markdown).await.unwrap();
    assert_eq!(imported, 2);

    let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
    let all = service.all().await.unwrap();
    assert_eq!(all.len(), 2);

    let dead: Vec<_> = all.iter().filter(|m| m.invalid_at.is_some()).collect();
    assert_eq!(
        dead.len(),
        1,
        "exactly one imported memory must be superseded, got: {all:?}"
    );
    let dead = dead[0];
    assert_eq!(dead.content, "The database is SQLite.");

    // Fresh rows: neither imported id may be reused from the source database.
    assert_ne!(dead.id, old_id);

    // The remapped pointer. It must not be the source successor's id, and it
    // must resolve, in *this* database, to the replacement content.
    let successor = dead.superseded_by.expect("successor must be recorded");
    assert_ne!(
        successor, new_id,
        "superseded_by must be remapped onto the imported successor, not carried over"
    );
    let resolved = service.get(successor).await.unwrap();
    assert_eq!(resolved.content, "The database is Postgres.");
    assert!(resolved.invalid_at.is_none());

    // The point of all of it: search answers with the current fact only.
    let hits = service
        .search(
            "database",
            SearchOptions {
                project: Some("p1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let contents: Vec<&str> = hits.iter().map(|h| h.memory.content.as_str()).collect();
    assert_eq!(contents, vec!["The database is Postgres."]);

    drop(service);
    for path in [&source_db, &target_db, &markdown] {
        if let Some(dir) = path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

/// A file whose `superseded_by` names a memory in no section of that file.
///
/// The memory must import as dead with no successor recorded. Dropping the
/// invalidation instead -- because the pointer could not be resolved -- would
/// bring back a fact the export recorded as dead, which is the exact failure
/// this round-tripping exists to prevent.
#[tokio::test]
async fn a_successor_missing_from_the_file_leaves_the_memory_dead() {
    let _env = ENV.lock().await;
    let target_db = temp_path("target.db");
    let markdown = temp_path("partial.md");

    // Hand-written: the successor uuid appears nowhere as a section heading,
    // which is what an export of a filtered subset would look like.
    let absent_successor = uuid::Uuid::new_v4();
    std::fs::write(
        &markdown,
        format!(
            "# mem8 export\n\n\
             ## {}\n\
             - project: p1\n\
             - kind: fact\n\
             - tags: []\n\
             - created: 2026-01-01T00:00:00+00:00\n\
             - superseded_by: {absent_successor}\n\
             - invalid_at: 2026-02-01T00:00:00+00:00\n\
             \n\
             The database is SQLite.\n\n",
            uuid::Uuid::new_v4()
        ),
    )
    .unwrap();

    std::env::set_var("MEM8_DB", format!("sqlite://{}", target_db.display()));
    let imported = mem8::cli::import(&markdown).await.unwrap();
    assert_eq!(imported, 1);

    let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
    let all = service.all().await.unwrap();
    assert_eq!(all.len(), 1);

    let m = &all[0];
    assert!(
        m.invalid_at.is_some(),
        "an unresolvable successor must not resurrect the memory, got: {m:?}"
    );
    assert_eq!(
        m.superseded_by, None,
        "no successor exists in this database, so none may be recorded"
    );

    let hits = service
        .search(
            "database",
            SearchOptions {
                project: Some("p1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "a dead memory must stay out of search, got: {hits:?}"
    );

    // Still retrievable by id, which is what invalidation rather than deletion
    // buys.
    assert_eq!(
        service.get(m.id).await.unwrap().content,
        "The database is SQLite."
    );

    drop(service);
    for path in [&target_db, &markdown] {
        if let Some(dir) = path.parent() {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

/// A store that delegates everything, but fails the Nth `supersede`.
///
/// Wrapping rather than mocking, so every other operation behaves exactly as
/// the real thing: the point is to observe what the *rest* of the import did
/// after one invalidation failed.
struct SupersedeFailsOnCall {
    inner: mem8::store::MemStore,
    fail_on: usize,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl mem8::store::Store for SupersedeFailsOnCall {
    async fn add(&self, new: mem8::model::NewMemory) -> mem8::error::Result<mem8::model::Memory> {
        self.inner.add(new).await
    }
    async fn get(&self, id: uuid::Uuid) -> mem8::error::Result<mem8::model::Memory> {
        self.inner.get(id).await
    }
    async fn update(
        &self,
        id: uuid::Uuid,
        u: mem8::model::MemoryUpdate,
    ) -> mem8::error::Result<mem8::model::Memory> {
        self.inner.update(id, u).await
    }
    async fn delete(&self, id: uuid::Uuid) -> mem8::error::Result<()> {
        self.inner.delete(id).await
    }
    async fn search(
        &self,
        q: mem8::model::SearchQuery,
    ) -> mem8::error::Result<Vec<mem8::model::SearchHit>> {
        self.inner.search(q).await
    }
    async fn all(&self) -> mem8::error::Result<Vec<mem8::model::Memory>> {
        self.inner.all().await
    }
    async fn vector_search(
        &self,
        q: mem8::model::VectorQuery,
    ) -> mem8::error::Result<Vec<mem8::model::SearchHit>> {
        self.inner.vector_search(q).await
    }
    async fn missing_embeddings(
        &self,
        limit: usize,
    ) -> mem8::error::Result<Vec<mem8::model::Memory>> {
        self.inner.missing_embeddings(limit).await
    }
    async fn set_embedding(&self, id: uuid::Uuid, e: &[f32]) -> mem8::error::Result<()> {
        self.inner.set_embedding(id, e).await
    }
    async fn supersede(
        &self,
        old: uuid::Uuid,
        new: Option<uuid::Uuid>,
        at: chrono::DateTime<chrono::Utc>,
    ) -> mem8::error::Result<()> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if n == self.fail_on {
            return Err(mem8::error::Mem8Error::Store(
                "simulated invalidation failure".into(),
            ));
        }
        self.inner.supersede(old, new, at).await
    }
}

/// One failing invalidation must not leave the *other* dead memories live.
///
/// Nothing spans import's two passes transactionally, so an early return on the
/// first failure would commit rows the file records as dead and leave them
/// live. That state is unrepairable: `supersede` is write-once, import always
/// creates fresh rows, and no other CLI path invalidates an existing row. So
/// every invalidation must be attempted, and the failures reported at the end.
#[tokio::test]
async fn one_failing_invalidation_does_not_leave_the_others_live() {
    // No `MEM8_DB` here: the store is injected, so this test needs no env lock.
    let markdown = temp_path("two-dead.md");

    // Two dead memories sharing one successor, so the file has two entries in
    // the second pass and failing the first still leaves the second to do.
    let dead_a = uuid::Uuid::new_v4();
    let dead_b = uuid::Uuid::new_v4();
    let successor = uuid::Uuid::new_v4();
    std::fs::write(
        &markdown,
        format!(
            "# mem8 export\n\n\
             ## {dead_a}\n- project: p1\n- kind: fact\n- tags: []\n\
             - created: 2026-01-01T00:00:00+00:00\n\
             - superseded_by: {successor}\n- invalid_at: 2026-02-01T00:00:00+00:00\n\
             \nThe database is SQLite.\n\n\
             ## {dead_b}\n- project: p1\n- kind: fact\n- tags: []\n\
             - created: 2026-01-02T00:00:00+00:00\n\
             - superseded_by: {successor}\n- invalid_at: 2026-02-01T00:00:00+00:00\n\
             \nThe database is MySQL.\n\n\
             ## {successor}\n- project: p1\n- kind: fact\n- tags: []\n\
             - created: 2026-01-03T00:00:00+00:00\n\
             \nThe database is Postgres.\n\n"
        ),
    )
    .unwrap();

    let store = SupersedeFailsOnCall {
        inner: mem8::store::MemStore::new(),
        fail_on: 1,
        calls: std::sync::atomic::AtomicUsize::new(0),
    };

    let text = std::fs::read_to_string(&markdown).unwrap();
    let err = mem8::cli::import_text(&store, &markdown, &text)
        .await
        .expect_err("a failed invalidation must not be reported as success");
    let message = err.to_string();

    let all = mem8::store::Store::all(&store).await.unwrap();
    assert_eq!(all.len(), 3, "every row is still written: {all:#?}");

    // The failure hit the first dead memory, so that one is live -- unavoidable
    // without a transaction, which is why the error must name it.
    let sqlite = all
        .iter()
        .find(|m| m.content == "The database is SQLite.")
        .unwrap();
    assert!(sqlite.invalid_at.is_none(), "the failing one stays live");
    assert!(
        message.contains(&sqlite.id.to_string()),
        "the error must name the memory left live, got: {message}"
    );

    // The property under test: the *second* dead memory was still invalidated,
    // rather than being abandoned by an early return.
    let mysql = all
        .iter()
        .find(|m| m.content == "The database is MySQL.")
        .unwrap();
    assert!(
        mysql.invalid_at.is_some(),
        "an unrelated dead memory must not be left live by another's failure: {mysql:?}"
    );
    let postgres = all
        .iter()
        .find(|m| m.content == "The database is Postgres.")
        .unwrap();
    assert_eq!(
        mysql.superseded_by,
        Some(postgres.id),
        "and its successor must still be remapped correctly"
    );

    // The message has to say the import partially applied, not just that
    // something failed -- and count the failure against the memories that were
    // actually invalidation candidates (2 here), not against all 3 imported.
    assert!(
        message.contains("imported 3 memories"),
        "the error must report how many imported, got: {message}"
    );
    assert!(
        message.contains("1 of the 2 recorded as superseded"),
        "the failure count must be out of the memories recorded as superseded, \
         not out of every memory imported, got: {message}"
    );

    if let Some(dir) = markdown.parent() {
        std::fs::remove_dir_all(dir).ok();
    }
}
