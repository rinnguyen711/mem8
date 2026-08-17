# Semantic search — Design

**Date:** 2026-08-17
**Status:** Scoped, not approved for implementation
**Depends on:** evidence from `~/.mem8/missed-searches.log`

## The problem

Search is keyword-only. Both backends require every term in a query to appear in
a memory, so a question phrased with different words than the memory finds
nothing:

```
"porter tokenizer"                      → 2 hits
"why did we pick the porter tokenizer"  → nothing
```

The stored memory says *"we **chose** the porter tokenizer"*. `pick` and `chose`
are different words. Stemming does not help — it normalises word forms (`run` →
`running`), not meaning.

The same gap defeats duplicate detection. Two memories recording the same
decision in different words score 0.14 word overlap, indistinguishable from
unrelated pairs at 0.09–0.15, which is why `DUPLICATE_THRESHOLD` sits at 0.8 and
catches only literal re-saves.

Both failures have one cause: nothing in the system represents meaning.

## Do not build this yet

`~/.mem8/missed-searches.log` records every search that returns nothing, with the
raw query beside its sanitized form. That log is the precondition for this work.

Read it after a week of real use and classify the misses:

- **Synonym-shaped** — the memory exists but uses different words. This design is
  the fix.
- **Absent content** — nothing was ever stored. No search improvement helps.
- **Wrong keywords** — the query was malformed or too broad. Better tool
  descriptions are the cheaper fix.

Only the first justifies a 100 MB binary and an embedding pass on every write.
One known failure and no frequency data is not enough to spend that.

## Non-goals

- Replacing keyword search. Exact identifiers (`auth-token`, `SqliteStore`,
  `f01839f`) are what an agent's memory is full of, and embeddings match those
  poorly — a search for `SqliteStore` will surface `PgStore` as similar.
- Reranking models, query expansion, or chunking. Memories are short.
- Making semantic search the default. It is opt-in.

## Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Embedding source | `fastembed` (local ONNX) | No API key, no network after first download, no daemon. Verified at v5.16.2, June 2026. |
| Model | BGE-small-en-v1.5 | fastembed's default; 384 dimensions, ~130 MB. Quality suits short factual sentences. |
| SQLite storage | `BLOB` + cosine in Rust | `sqlite-vec` is 0.1.10-alpha and documents expected breaking changes to its SQL API and storage format. A stored embedding that must be regenerated on every format change is not persistence. |
| Postgres storage | pgvector | Mature and stable. Its HNSW index earns nothing at this scale but costs nothing to use. |
| Retrieval | Hybrid — keyword and vector, merged | Keyword keeps exact identifiers findable; vector catches reworded queries. Either alone regresses one of the two. |
| Distribution | Cargo feature `semantic`, off by default | The binary grows from ~10 MB to ~100 MB+. Users who do not need it should not carry it. |

### Why brute force is enough

A linear scan comparing a query vector against every stored vector is O(n) in the
number of memories. At 384 dimensions and a realistic ceiling of a few thousand
memories per project, that is well under a millisecond — `sqlite-vec` itself is
brute-force at this size. An ANN index only earns its complexity in the hundreds
of thousands, which this design does not target.

## Architecture

The existing four layers are unchanged. Semantic search adds one module and
touches two others.

```
  mcp/          unchanged — the tool surface does not change
    ↓
  core.rs       embeds on write; merges keyword and vector results on read
    ↓
  embed/        NEW — wraps fastembed, owns the model lifecycle
    ↓
  store/        gains vector_search; SQLite scans, Postgres delegates to pgvector
```

`core` gains a dependency on `embed`, and `store` gains one method. Nothing else
moves.

### The embedder

```rust
// src/embed/mod.rs
pub struct Embedder { /* fastembed::TextEmbedding */ }

impl Embedder {
    /// Load the model, downloading it on first use.
    pub fn load() -> Result<Self>;
    /// Embed one text. 384 floats.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Embed many in one pass; far cheaper than repeated single calls.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
}

pub const EMBEDDING_DIM: usize = 384;
```

Model loading is slow (hundreds of milliseconds) and must happen once per
process, not once per call. The MCP server is long-lived, so the embedder is
constructed at startup and shared.

When the `semantic` feature is off, `Embedder` does not exist and `core` compiles
without it.

### Storage

The `embedding` column already exists in both schemas and is always NULL.

**SQLite** — store the raw little-endian `f32` bytes in the existing `BLOB`:

```rust
fn encode(v: &[f32]) -> Vec<u8>;
fn decode(b: &[u8]) -> Option<Vec<f32>>;  // None if the length is not a multiple of 4
```

Search loads candidate rows (filtered by project and kind first, so the scan is
over a subset) and ranks by cosine similarity in Rust.

**Postgres** — replace the placeholder with a real vector column:

```sql
CREATE EXTENSION IF NOT EXISTS vector;
ALTER TABLE memories ADD COLUMN embedding vector(384);
CREATE INDEX ON memories USING hnsw (embedding vector_cosine_ops);
```

This is the project's first real migration, and it exposes a known gap:
`SqliteStore` checks `PRAGMA user_version` and refuses a database newer than the
binary, while `PgStore` has no equivalent. That guard must be added here, not
deferred again.

### The `Store` trait gains one method

```rust
/// Memories ranked by embedding similarity. Filters apply before ranking,
/// exactly as in `search`.
async fn vector_search(&self, query: VectorQuery) -> Result<Vec<SearchHit>>;
```

`MemStore` implements it with the same cosine arithmetic, so `core` tests keep
working without a real backend.

### Merging results

Keyword and vector searches return different score scales — BM25, `ts_rank`, and
cosine similarity are not comparable. Ranks are.

Use Reciprocal Rank Fusion, which combines ranked lists without needing
comparable scores:

```
score(memory) = Σ  1 / (k + rank_in_list)      k = 60
```

A memory found by both searches outranks one found by either alone, which is the
desired behaviour: agreement between two independent methods is the strongest
signal available.

`k = 60` is the standard value from the RRF literature and needs no tuning here.

## Behaviour changes

**Writes cost an embedding pass.** Tens of milliseconds on CPU. `add_memory`
becomes measurably slower.

**First run downloads the model.** ~130 MB to `./.fastembed_cache`, once.
Offline afterwards. This must be surfaced clearly rather than appearing as a
hang.

**Existing memories have no embedding.** They remain findable by keyword and
invisible to vector search until backfilled. A `mem8 reindex` command embeds
every memory lacking one; it is the only way an existing database gains semantic
search.

**Duplicate detection improves for free.** `find_duplicate` currently compares
word overlap. With embeddings available it can compare cosine similarity, which
is what would actually catch the porter-tokenizer pair. That is a follow-up, not
part of this change, and it needs its own threshold measured against real data —
the 0.8 word-overlap threshold does not transfer.

## Testing

**Determinism.** Embeddings are floating point and model-dependent. Tests must
not assert exact vectors. Assert relationships instead: that "we chose Rust" is
closer to "we picked Rust" than to "run cargo fmt".

**The contract suite gains a semantic case**, gated on the feature, run against
both backends. It proves the two implementations agree on which memories are
similar — the same protection the keyword contract provides today.

**The failing query becomes a test.** `"why did we pick the porter tokenizer"`
must find a memory recorded as `"we chose the porter tokenizer"`. That query is
the reason this design exists; it belongs in the suite.

**Exact identifiers must not regress.** A search for `SqliteStore` must still
rank the memory containing `SqliteStore` above one containing `PgStore`. This is
the risk hybrid retrieval exists to manage, and it needs a test that would fail
if vector results swamped keyword results.

**Model loading is mocked in unit tests.** Downloading 130 MB in CI is
unacceptable; the embedder is behind a trait so tests substitute a deterministic
fake.

## Rollout

1. `embed` module and the feature flag, with no wiring — provable in isolation.
2. `vector_search` on `MemStore` and `SqliteStore`, plus contract tests.
3. Embedding on write, and `mem8 reindex` for existing rows.
4. RRF merging in `core`, behind the flag.
5. pgvector for the Postgres backend, with the schema-version guard.

Each step leaves the tree green and the feature off by default.

## What would make this the wrong choice

- The miss log shows misses are mostly absent content, not synonyms.
- A week of use produces almost no misses at all.
- Exact-identifier search regresses in a way hybrid retrieval cannot recover.

Any of those means the honest answer is to keep keyword search and improve the
tool descriptions instead.
