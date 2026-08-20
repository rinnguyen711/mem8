//! Vector search against a real Postgres with pgvector.
//!
//! The `MemStore` cosine path is exercised by unit tests, but the SQL —
//! `<=>`, the HNSW index, the `vector(384)` binding, the filter interaction —
//! only exists here. Two backends agreeing in Rust proves nothing about what
//! Postgres does with the query.
//!
//! Opt-in: needs `MEM8_TEST_PG` and the `semantic` feature. Deterministic
//! vectors are constructed by hand rather than by a model, so this test needs
//! no download and asserts exact ranking rather than approximate similarity.

#![cfg(feature = "semantic")]

use mem8::embed::EMBEDDING_DIM;
use mem8::error::Mem8Error;
use mem8::model::{Kind, NewMemory, VectorQuery};
use mem8::store::postgres::PgStore;
use mem8::store::Store;

/// A store plus a project name unique to this test.
///
/// Deliberately not `reset_for_tests`: truncating the shared table would delete
/// rows the other tests in this file are using, and `cargo test` runs them in
/// parallel. Isolating by project instead means each test sees only its own
/// memories, which is exactly the scoping the code under test already enforces.
macro_rules! store {
    () => {
        match std::env::var("MEM8_TEST_PG") {
            Ok(url) => {
                let store = PgStore::connect(&url).await.unwrap();
                let scope = format!("vec_{}", uuid::Uuid::new_v4().simple());
                (store, scope)
            }
            Err(_) => {
                eprintln!("skipping: MEM8_TEST_PG not set");
                return;
            }
        }
    };
}

/// A unit vector pointing along one axis.
///
/// Two such vectors are identical when the axis matches and orthogonal when it
/// does not, so "nearest" is exact and the assertions carry no tolerance.
fn axis(n: usize) -> Vec<f32> {
    let mut v = vec![0.0; EMBEDDING_DIM];
    v[n % EMBEDDING_DIM] = 1.0;
    v
}

/// A vector at strictly increasing distance from `axis(0)` as `i` grows, so
/// `ORDER BY distance ASC` has one unambiguous order.
///
/// `axis(0)` for every row would make all five distances 0 and leave the
/// ordering to Postgres's discretion. A LIMIT test built on a tie proves
/// nothing: whether the superseded rows occupy the first slots is then luck,
/// and a naive Rust post-filter passes it.
fn tilt(i: usize) -> Vec<f32> {
    let mut v = vec![0.0; EMBEDDING_DIM];
    v[0] = 1.0;
    v[1 + i] = 0.01 * (i as f32 + 1.0);
    v
}

fn memory(project: &str, content: &str, embedding: Option<Vec<f32>>) -> NewMemory {
    NewMemory {
        project: project.into(),
        kind: Kind::Fact,
        content: content.into(),
        tags: vec![],
        embedding,
    }
}

fn query(embedding: Vec<f32>, project: &str) -> VectorQuery {
    VectorQuery {
        embedding,
        project: Some(project.into()),
        global: false,
        kind: None,
        tags: vec![],
        limit: 10,
        include_superseded: false,
        as_of: None,
    }
}

#[tokio::test]
async fn nearest_vector_ranks_first() {
    let (store, p1) = store!();

    store
        .add(memory(&p1, "on axis 0", Some(axis(0))))
        .await
        .unwrap();
    let target = store
        .add(memory(&p1, "on axis 7", Some(axis(7))))
        .await
        .unwrap();
    store
        .add(memory(&p1, "on axis 11", Some(axis(11))))
        .await
        .unwrap();

    let hits = store.vector_search(query(axis(7), &p1)).await.unwrap();

    assert_eq!(
        hits[0].memory.id, target.id,
        "the identical vector must rank first"
    );
    // Score is reported as similarity (1 - cosine distance), so an exact match
    // is 1.0 and orthogonal vectors are 0.0.
    assert!(
        (hits[0].score - 1.0).abs() < 1e-5,
        "exact match should score ~1.0, got {}",
        hits[0].score
    );
    assert!(
        hits[1].score < 0.5,
        "orthogonal vectors should score far lower"
    );
}

#[tokio::test]
async fn memories_without_an_embedding_are_skipped() {
    let (store, p1) = store!();

    store
        .add(memory(&p1, "never embedded", None))
        .await
        .unwrap();
    let embedded = store
        .add(memory(&p1, "embedded", Some(axis(3))))
        .await
        .unwrap();

    let hits = store.vector_search(query(axis(3), &p1)).await.unwrap();

    assert_eq!(
        hits.len(),
        1,
        "a NULL embedding is unrepresented, not distant"
    );
    assert_eq!(hits[0].memory.id, embedded.id);
}

#[tokio::test]
async fn vector_search_respects_project_scope() {
    let (store, p1) = store!();

    let p2 = format!("{p1}_other");
    let mine = store
        .add(memory(&p1, "same vector", Some(axis(5))))
        .await
        .unwrap();
    store
        .add(memory(&p2, "same vector", Some(axis(5))))
        .await
        .unwrap();

    let hits = store.vector_search(query(axis(5), &p1)).await.unwrap();

    assert_eq!(hits.len(), 1, "scope must be applied before ranking");
    assert_eq!(hits[0].memory.id, mine.id);
}

#[tokio::test]
async fn vector_search_applies_kind_and_tag_filters() {
    let (store, p1) = store!();

    store
        .add(memory(&p1, "a plain fact", Some(axis(2))))
        .await
        .unwrap();
    let wanted = store
        .add(NewMemory {
            kind: Kind::Decision,
            tags: vec!["lang".into(), "tools".into()],
            ..memory(&p1, "a tagged decision", Some(axis(2)))
        })
        .await
        .unwrap();

    let by_kind = VectorQuery {
        kind: Some(Kind::Decision),
        ..query(axis(2), &p1)
    };
    let hits = store.vector_search(by_kind).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].memory.id, wanted.id);

    // Tags use AND semantics, matching keyword search.
    let by_tags = VectorQuery {
        tags: vec!["lang".into(), "tools".into()],
        ..query(axis(2), &p1)
    };
    assert_eq!(store.vector_search(by_tags).await.unwrap().len(), 1);

    let missing_tag = VectorQuery {
        tags: vec!["lang".into(), "absent".into()],
        ..query(axis(2), &p1)
    };
    assert!(store.vector_search(missing_tag).await.unwrap().is_empty());
}

#[tokio::test]
async fn vector_search_honours_the_limit() {
    let (store, p1) = store!();
    for i in 0..5 {
        store
            .add(memory(&p1, &format!("memory {i}"), Some(axis(i))))
            .await
            .unwrap();
    }

    let limited = VectorQuery {
        limit: 2,
        ..query(axis(0), &p1)
    };
    assert_eq!(store.vector_search(limited).await.unwrap().len(), 2);
}

#[tokio::test]
async fn global_search_crosses_projects() {
    let (store, p1) = store!();
    let p2 = format!("{p1}_other");

    // A vector unique to this test, so a global search cannot be satisfied by
    // another test's rows -- the table is shared and every test writes to it.
    let unique = axis(97);
    let first = store
        .add(memory(&p1, "first", Some(unique.clone())))
        .await
        .unwrap();
    let second = store
        .add(memory(&p2, "second", Some(unique.clone())))
        .await
        .unwrap();

    let global = VectorQuery {
        global: true,
        project: None,
        limit: 50,
        ..query(unique, &p1)
    };
    let found: Vec<_> = store
        .vector_search(global)
        .await
        .unwrap()
        .into_iter()
        .filter(|h| h.memory.id == first.id || h.memory.id == second.id)
        .collect();

    assert_eq!(found.len(), 2, "global search must reach both projects");
    assert!(found.iter().all(|h| (h.score - 1.0).abs() < 1e-5));
}

#[tokio::test]
async fn backfill_finds_then_fills_missing_embeddings() {
    let (store, p1) = store!();

    let bare = store
        .add(memory(&p1, "needs an embedding", None))
        .await
        .unwrap();
    let embedded = store
        .add(memory(&p1, "already has one", Some(axis(4))))
        .await
        .unwrap();

    // `missing_embeddings` is table-wide by design -- `mem8 reindex` backfills
    // every project at once -- so this asserts membership rather than length.
    // Other tests are writing to the same table in parallel.
    let missing = store.missing_embeddings(1000).await.unwrap();
    assert!(
        missing.iter().any(|m| m.id == bare.id),
        "the unembedded row must be listed"
    );
    assert!(
        !missing.iter().any(|m| m.id == embedded.id),
        "an already-embedded row must not be listed"
    );

    store.set_embedding(bare.id, &axis(9)).await.unwrap();

    let after = store.missing_embeddings(1000).await.unwrap();
    assert!(
        !after.iter().any(|m| m.id == bare.id),
        "backfill must remove the row from the queue"
    );

    let hits = store.vector_search(query(axis(9), &p1)).await.unwrap();
    assert_eq!(
        hits[0].memory.id, bare.id,
        "a backfilled memory becomes findable"
    );
}

#[tokio::test]
async fn backfill_does_not_change_the_updated_timestamp() {
    // Indexing is not editing. Moving `updated_at` would misreport when the
    // user last changed the memory.
    let (store, p1) = store!();
    let m = store.add(memory(&p1, "unedited", None)).await.unwrap();

    store.set_embedding(m.id, &axis(6)).await.unwrap();

    let after = store.get(m.id).await.unwrap();
    assert_eq!(
        after.updated_at, m.updated_at,
        "backfill must not touch updated_at"
    );
    assert_eq!(after.content, "unedited");
}

#[tokio::test]
async fn setting_an_embedding_on_a_missing_id_is_not_found() {
    let (store, _scope) = store!();
    let err = store
        .set_embedding(uuid::Uuid::new_v4(), &axis(0))
        .await
        .unwrap_err();
    assert!(matches!(err, Mem8Error::NotFound(_)), "got: {err}");
}

#[tokio::test]
async fn updating_content_can_replace_the_embedding() {
    let (store, p1) = store!();
    let m = store
        .add(memory(&p1, "original", Some(axis(1))))
        .await
        .unwrap();

    store
        .update(
            m.id,
            mem8::model::MemoryUpdate {
                content: Some("revised".into()),
                embedding: Some(axis(8)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // The new vector is the one that matches now.
    let hits = store.vector_search(query(axis(8), &p1)).await.unwrap();
    assert_eq!(hits[0].memory.id, m.id);
    assert!((hits[0].score - 1.0).abs() < 1e-5);

    // And the old one no longer does.
    let stale = store.vector_search(query(axis(1), &p1)).await.unwrap();
    assert!(
        stale[0].score < 0.5,
        "the replaced vector should no longer match"
    );
}

#[tokio::test]
async fn an_update_that_omits_the_embedding_keeps_the_stored_one() {
    // Editing tags must not silently discard the vector -- COALESCE, not
    // overwrite-with-NULL.
    let (store, p1) = store!();
    let m = store
        .add(memory(&p1, "keep my vector", Some(axis(12))))
        .await
        .unwrap();

    store
        .update(
            m.id,
            mem8::model::MemoryUpdate {
                tags: Some(vec!["new-tag".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let hits = store.vector_search(query(axis(12), &p1)).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "the embedding must survive an unrelated update"
    );
    assert!((hits[0].score - 1.0).abs() < 1e-5);
}

/// Semantic search must hide superseded facts exactly as keyword search does.
///
/// A `vector_search` that ignored supersession would surface dead facts that
/// `search` correctly hides — the same memory reachable or not depending only
/// on which search the caller happened to use.
#[tokio::test]
async fn vector_search_applies_the_temporal_predicate() {
    let (store, p1) = store!();

    let old = store
        .add(memory(&p1, "storage is sqlite", Some(axis(0))))
        .await
        .unwrap();
    let replacement = store
        .add(memory(&p1, "storage is postgres", Some(axis(0))))
        .await
        .unwrap();

    let at = old.created_at + chrono::Duration::seconds(10);
    store
        .supersede(old.id, Some(replacement.id), at)
        .await
        .unwrap();

    // Default: hidden.
    let live = store.vector_search(query(axis(0), &p1)).await.unwrap();
    let live_ids: Vec<_> = live.iter().map(|h| h.memory.id).collect();
    assert!(
        !live_ids.contains(&old.id),
        "vector_search must hide a superseded memory"
    );
    assert!(live_ids.contains(&replacement.id));

    // include_superseded: both.
    let both = store
        .vector_search(VectorQuery {
            include_superseded: true,
            ..query(axis(0), &p1)
        })
        .await
        .unwrap();
    assert_eq!(both.len(), 2);

    // as_of before the invalidation: the old fact was still believed. Offsets
    // are whole seconds — TIMESTAMPTZ holds microseconds, so a sub-microsecond
    // offset would compare differently here than in SQLite's nanosecond text.
    let before = store
        .vector_search(VectorQuery {
            as_of: Some(old.created_at + chrono::Duration::seconds(5)),
            ..query(axis(0), &p1)
        })
        .await
        .unwrap();
    assert!(
        before.iter().any(|h| h.memory.id == old.id),
        "as_of before the invalidation must include the old fact"
    );

    // Exactly at the invalidation it is already dead: the predicate is
    // `invalid_at > T`.
    let at_boundary = store
        .vector_search(VectorQuery {
            as_of: Some(at),
            ..query(axis(0), &p1)
        })
        .await
        .unwrap();
    assert!(!at_boundary.iter().any(|h| h.memory.id == old.id));
}

/// The temporal predicate must be applied in SQL, before `LIMIT`.
///
/// `LIMIT` lives in the Postgres SQL (unlike SQLite, which truncates in Rust
/// after tag filtering). A predicate applied in Rust instead would truncate to
/// `limit` first and then remove superseded rows, so a limited search would
/// return fewer than `limit` live hits whenever dead rows occupied slots.
#[tokio::test]
async fn vector_search_fills_the_limit_with_live_rows() {
    let (store, p1) = store!();

    let mut added = vec![];
    for i in 0..5 {
        added.push(
            store
                .add(memory(&p1, &format!("memory {i}"), Some(tilt(i))))
                .await
                .unwrap(),
        );
    }

    // Supersede the two NEAREST rows, so they are exactly the ones an
    // unfiltered `LIMIT 3` would return. With the predicate in SQL they are
    // never selected and the limit fills with rows 2, 3 and 4; with a Rust
    // post-filter the SQL returns rows 0, 1, 2 and the filter cuts it to one.
    let at = added[0].created_at + chrono::Duration::seconds(10);
    store.supersede(added[0].id, None, at).await.unwrap();
    store.supersede(added[1].id, None, at).await.unwrap();

    let hits = store
        .vector_search(VectorQuery {
            limit: 3,
            ..query(axis(0), &p1)
        })
        .await
        .unwrap();

    assert_eq!(
        hits.len(),
        3,
        "LIMIT must be filled with live rows, not reduced by a post-filter"
    );
    assert!(hits.iter().all(|h| h.memory.invalid_at.is_none()));

    // The three live rows, nearest first. Naming them pins the ordering as
    // well as the count, so a tie could not satisfy this by accident.
    let got: Vec<_> = hits.iter().map(|h| h.memory.id).collect();
    assert_eq!(got, vec![added[2].id, added[3].id, added[4].id]);
}
