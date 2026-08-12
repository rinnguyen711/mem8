use mem8::core::Memory8;
use mem8::model::Kind;

fn temp_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("mem8-rt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// `export` followed by `import` must reproduce the same memory set. This is
/// what keeps the markdown format honest.
///
/// This test mutates the `MEM8_DB` process environment, so it must be the
/// only test in its file. Do not add further tests here.
#[tokio::test]
async fn export_then_import_reproduces_every_memory() {
    let source_db = temp_path("source.db");
    let target_db = temp_path("target.db");
    let markdown = temp_path("memories.md");

    // Populate the source database.
    std::env::set_var("MEM8_DB", format!("sqlite://{}", source_db.display()));
    {
        let service = Memory8::new(mem8::store::open_from_env().await.unwrap());
        service
            .add("We chose Rust.", Kind::Decision, vec!["lang".into()], Some("p1".into()))
            .await
            .unwrap();
        service
            .add("Tests use cargo test.", Kind::Convention, vec![], Some("p1".into()))
            .await
            .unwrap();
        service
            .add("Multi\n\nparagraph body.", Kind::Fact, vec!["a".into(), "b".into()], Some("p2".into()))
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
