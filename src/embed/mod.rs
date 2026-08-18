//! Text embeddings for semantic search.
//!
//! Keyword search requires a query's words to appear in the memory. A question
//! phrased differently from the stored text finds nothing, however close the
//! meaning: "why did we pick the porter tokenizer" does not find "we chose the
//! porter tokenizer". Embeddings put both into a vector space where closeness
//! is similarity of meaning rather than of spelling.
//!
//! Everything here is behind the `semantic` feature. With it off the module is
//! still compiled — the trait, the dimension, and the fake are cheap and let
//! `core` keep one code path — but no model and no ONNX runtime are pulled in.

use crate::error::Result;

/// Dimensions produced by the embedding model.
///
/// Fixed at the Postgres column type (`vector(384)`), so a model whose output
/// differs cannot be stored. `Embedder::load` checks this at startup rather
/// than letting every insert fail with a type error that names no cause.
pub const EMBEDDING_DIM: usize = 384;

/// Something that turns text into a vector.
///
/// A trait rather than a bare struct so tests can substitute a deterministic
/// fake: the real model is a ~130 MB download, which no unit test should need.
pub trait Embed: Send + Sync {
    /// Embed several texts in one pass.
    ///
    /// The primitive, not a convenience: the underlying model batches, and one
    /// call per text is markedly slower than one call for all of them.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Embed a single text.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut out = self.embed_batch(&[text])?;
        if out.len() != 1 {
            return Err(crate::error::Mem8Error::Store(format!(
                "embedder returned {} vectors for 1 text",
                out.len()
            )));
        }
        Ok(out.remove(0))
    }
}

/// Cosine similarity, in `[-1, 1]`; higher is more similar.
///
/// Returns 0.0 when either vector has no magnitude or the lengths disagree,
/// which is the neutral answer: an unusable comparison should rank a memory
/// neither above nor below the others rather than erroring a whole search.
///
/// **Only meaningful relatively.** Measured against BGE-small on real mem8
/// content, a memory and a reworded question about it score 0.81, a question
/// sharing no distinctive keyword scores 0.60, and completely unrelated
/// sentences still score 0.42–0.48. The model compresses everything into a
/// narrow band well above zero, so ranking works but an absolute threshold does
/// not: 0.5 is "unrelated", not "half similar". Anything that needs a cutoff —
/// semantic duplicate detection, for one — has to calibrate against real data
/// rather than pick a round number.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(feature = "semantic")]
mod real {
    use super::{Embed, EMBEDDING_DIM};
    use crate::error::{Mem8Error, Result};
    use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

    /// A local ONNX embedding model.
    ///
    /// BGE-small-en-v1.5: 384 dimensions, ~130 MB, and fastembed's default.
    /// Chosen for running locally with no API key and no network after the
    /// first download — a memory tool that cannot work offline is not
    /// persistent memory.
    /// The model is behind a `Mutex` because fastembed's `embed` takes
    /// `&mut self`, while `Embed` is shared across the server behind an `Arc`.
    /// Serialising is not a real cost here: ONNX inference is CPU-bound and
    /// already internally parallel, so concurrent calls would contend for the
    /// same cores rather than finish sooner.
    pub struct Embedder {
        model: std::sync::Mutex<TextEmbedding>,
    }

    impl Embedder {
        /// Load the model, downloading it on first use.
        ///
        /// Slow — hundreds of milliseconds, plus the download the first time.
        /// Call once per process and share the result; the MCP server is
        /// long-lived, so this happens at startup.
        pub fn load() -> Result<Self> {
            let mut model = TextEmbedding::try_new(
                TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                    .with_show_download_progress(true),
            )
            .map_err(|e| {
                Mem8Error::Store(format!(
                    "could not load the embedding model: {e}. \
                     The first run downloads roughly 130 MB to ./.fastembed_cache; \
                     after that it works offline."
                ))
            })?;

            let probe = model
                .embed(vec!["dimension probe"], None)
                .map_err(|e| Mem8Error::Store(format!("embedding model failed on load: {e}")))?;

            // Catch a model/schema mismatch here rather than at the first
            // INSERT, where Postgres would reject the value with an error that
            // names neither the model nor the expected size.
            match probe.first() {
                Some(v) if v.len() == EMBEDDING_DIM => Ok(Self {
                    model: std::sync::Mutex::new(model),
                }),
                Some(v) => Err(Mem8Error::Store(format!(
                    "embedding model produced {} dimensions, but mem8 stores {EMBEDDING_DIM}; \
                     the database column and the model must agree",
                    v.len()
                ))),
                None => Err(Mem8Error::Store(
                    "embedding model returned nothing for a probe text".into(),
                )),
            }
        }
    }

    impl Embed for Embedder {
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let mut model = self
                .model
                .lock()
                .map_err(|_| Mem8Error::Store("the embedding model lock is poisoned".into()))?;
            model
                .embed(texts, None)
                .map_err(|e| Mem8Error::Store(format!("embedding failed: {e}")))
        }
    }
}

#[cfg(feature = "semantic")]
pub use real::Embedder;

/// A deterministic stand-in for the real model.
///
/// Hashes each word to a dimension and counts it, so texts sharing words land
/// near each other and texts sharing none stay apart. That is enough to
/// exercise ranking, merging, and storage without a 130 MB download; it is not
/// an approximation of semantic similarity and must never be used to assert
/// that two differently-worded sentences are close. Only the real model can
/// show that, which is why those tests are opt-in.
pub struct FakeEmbedder;

impl Embed for FakeEmbedder {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let mut v = vec![0.0f32; EMBEDDING_DIM];
                for word in text.split_whitespace() {
                    let word = word.to_lowercase();
                    // FNV-1a, inline: a stable hash across platforms and runs,
                    // which `DefaultHasher` does not promise.
                    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
                    for byte in word.bytes() {
                        hash ^= byte as u64;
                        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                    }
                    v[(hash as usize) % EMBEDDING_DIM] += 1.0;
                }
                v
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_opposite_vectors_is_negative_one() {
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_degenerate_input_without_panicking() {
        // Mismatched lengths, empties, and zero vectors all reduce to "no
        // information", which must rank neutrally rather than divide by zero.
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn fake_embedder_is_deterministic_and_correctly_sized() {
        let e = FakeEmbedder;
        let a = e.embed_one("we chose rust").unwrap();
        let b = e.embed_one("we chose rust").unwrap();
        assert_eq!(a.len(), EMBEDDING_DIM);
        assert_eq!(a, b, "the same text must always embed identically");
    }

    #[test]
    fn fake_embedder_separates_shared_words_from_unrelated_text() {
        let e = FakeEmbedder;
        let base = e.embed_one("we chose the porter tokenizer").unwrap();
        let overlapping = e.embed_one("we chose the porter stemmer").unwrap();
        let unrelated = e.embed_one("kubernetes ingress annotations").unwrap();

        assert!(
            cosine_similarity(&base, &overlapping) > cosine_similarity(&base, &unrelated),
            "shared words must score closer than none"
        );
    }

    #[test]
    fn embed_batch_preserves_order_and_count() {
        let e = FakeEmbedder;
        let out = e.embed_batch(&["first", "second", "third"]).unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], e.embed_one("first").unwrap());
        assert_eq!(out[2], e.embed_one("third").unwrap());
    }

    #[test]
    fn embed_batch_of_nothing_is_empty_not_an_error() {
        assert!(FakeEmbedder.embed_batch(&[]).unwrap().is_empty());
    }
}
