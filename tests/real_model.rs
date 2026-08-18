//! The real embedding model, against the queries this feature was built for.
//!
//! Everything else in the suite uses deterministic fake vectors, which prove
//! plumbing but say nothing about whether embeddings actually capture meaning.
//! Only the real model can show that, and it costs a ~130 MB download — so
//! these are opt-in twice over: the `semantic` feature, plus `MEM8_TEST_EMBED=1`.
//!
//! ```bash
//! MEM8_TEST_EMBED=1 cargo test --features semantic --test real_model
//! ```

#![cfg(feature = "semantic")]

use mem8::core::Memory8;
use mem8::embed::{cosine_similarity, Embed, Embedder, EMBEDDING_DIM};
use mem8::model::Kind;
use mem8::store::MemStore;
use std::sync::Arc;

/// Load the model, or skip. The download makes this unsuitable for CI.
macro_rules! embedder {
    () => {
        match std::env::var("MEM8_TEST_EMBED") {
            Ok(v) if v == "1" => match Embedder::load() {
                Ok(e) => e,
                Err(e) => panic!("could not load the embedding model: {e}"),
            },
            _ => {
                eprintln!("skipping: set MEM8_TEST_EMBED=1 to run against the real model");
                return;
            }
        }
    };
}

#[test]
fn the_model_produces_the_dimension_the_schema_expects() {
    let embedder = embedder!();
    let v = embedder.embed_one("a sentence").unwrap();
    assert_eq!(
        v.len(),
        EMBEDDING_DIM,
        "the model and the vector(384) column must agree"
    );
}

/// The failure that motivated this whole feature.
///
/// Recorded in the design spec: searching "why did we pick the porter
/// tokenizer" found nothing, because the memory says "chose" and the query says
/// "pick". Keyword search cannot bridge that; this is the test that says
/// embeddings can.
#[test]
fn a_reworded_question_is_closer_than_an_unrelated_sentence() {
    let embedder = embedder!();

    let memory = embedder
        .embed_one("We chose the porter tokenizer so both backends stem identically.")
        .unwrap();
    let reworded = embedder
        .embed_one("why did we pick the porter tokenizer")
        .unwrap();
    let unrelated = embedder
        .embed_one("Run cargo fmt before every commit.")
        .unwrap();

    let close = cosine_similarity(&memory, &reworded);
    let far = cosine_similarity(&memory, &unrelated);

    assert!(
        close > far,
        "a reworded question must be closer than an unrelated sentence \
         (reworded {close:.3}, unrelated {far:.3})"
    );
}

/// Exact identifiers are what an agent's memory is full of, and embeddings
/// match them poorly — `SqliteStore` and `PgStore` are near-identical strings
/// describing different things. This is the risk hybrid retrieval exists to
/// manage, so it is asserted rather than assumed.
#[tokio::test]
async fn an_exact_identifier_still_ranks_first() {
    let embedder = embedder!();

    let service = Memory8::with_embedder(Arc::new(MemStore::new()), Arc::new(embedder));

    let sqlite = service
        .add(
            "SqliteStore keeps its full-text index in an FTS5 virtual table.",
            Kind::Fact,
            vec![],
            Some("p1".into()),
        )
        .await
        .unwrap();
    service
        .add(
            "PgStore keeps its full-text index in a GIN index over to_tsvector.",
            Kind::Fact,
            vec![],
            Some("p1".into()),
        )
        .await
        .unwrap();

    let hits = service
        .search("SqliteStore", Some("p1".into()), false, None, vec![], None)
        .await
        .unwrap();

    assert_eq!(
        hits[0].memory.id, sqlite.id,
        "an exact identifier must not be displaced by a semantically similar memory; got: {}",
        hits[0].memory.content
    );
}

/// Hybrid retrieval should find a memory whose wording shares no distinctive
/// keyword with the query — the case keyword search cannot serve at all.
#[tokio::test]
async fn semantic_search_finds_what_keywords_miss() {
    let embedder = embedder!();

    let service = Memory8::with_embedder(Arc::new(MemStore::new()), Arc::new(embedder));

    let target = service
        .add(
            "We chose the porter tokenizer so both backends stem identically.",
            Kind::Decision,
            vec![],
            Some("p1".into()),
        )
        .await
        .unwrap();

    // Distractors, so a single-memory store cannot make this pass trivially.
    for filler in [
        "Run cargo fmt before every commit.",
        "The MCP server speaks over stdio.",
        "Memories are scoped to the git root directory name.",
    ] {
        service
            .add(filler, Kind::Convention, vec![], Some("p1".into()))
            .await
            .unwrap();
    }

    let hits = service
        .search(
            "why did we pick that word splitter",
            Some("p1".into()),
            false,
            None,
            vec![],
            None,
        )
        .await
        .unwrap();

    assert!(
        hits.iter().any(|h| h.memory.id == target.id),
        "the reworded query should surface the memory; got: {:?}",
        hits.iter().map(|h| &h.memory.content).collect::<Vec<_>>()
    );
}
