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

async fn add_decision(store: &SqliteStore, content: &str) -> mem8::model::Memory {
    store
        .add(NewMemory {
            project: "p1".into(),
            kind: Kind::Decision,
            content: content.into(),
            tags: vec![],
            ..Default::default()
        })
        .await
        .unwrap()
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
    let fresh = add_decision(&store, "we chose postgres").await;
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

/// Invalidation is write-once, enforced in SQL rather than in `core`.
///
/// Reachable from import, which calls `supersede` on rows that may already
/// carry an `invalid_at` parsed out of the export file. Moving that timestamp
/// forward would make the memory read as live for any `as_of` between the two
/// instants, so the second call must be refused outright.
#[tokio::test]
async fn superseding_twice_is_rejected_and_keeps_the_first_timestamp() {
    let dir = std::env::temp_dir().join(format!("mem8-mig3-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("v1.db");
    write_v1_database(&path);

    let store = SqliteStore::open(&path).unwrap();
    let original = store.all().await.unwrap().remove(0);

    let first = add_decision(&store, "we chose postgres").await;
    let second = add_decision(&store, "we chose something else").await;

    let at = chrono::Utc::now();
    store.supersede(original.id, Some(first.id), at).await.unwrap();

    let err = store
        .supersede(
            original.id,
            Some(second.id),
            at + chrono::Duration::hours(1),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, mem8::error::Mem8Error::InvalidInput(_)),
        "a second supersede must be InvalidInput, not NotFound or Ok: {err:?}"
    );

    // The original invalidation survived untouched -- this is the property that
    // keeps `as_of` answers stable.
    let got = store.get(original.id).await.unwrap();
    assert_eq!(got.invalid_at, Some(at));
    assert_eq!(got.superseded_by, Some(first.id));

    // A genuinely missing id is still NotFound, so the new branch did not
    // swallow that case into InvalidInput.
    let ghost = store
        .supersede(uuid::Uuid::new_v4(), Some(first.id), at)
        .await
        .unwrap_err();
    assert!(
        matches!(ghost, mem8::error::Mem8Error::NotFound(_)),
        "an unknown id must stay NotFound: {ghost:?}"
    );

    drop(store);
    std::fs::remove_dir_all(&dir).ok();
}
