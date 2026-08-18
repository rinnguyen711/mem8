# Semantic search on Postgres — Implementation plan

**Date:** 2026-08-17
**Supersedes scope of:** `specs/2026-08-17-semantic-search-design.md`
**Status:** approved for implementation

## What changed from the design

The design was written before this scope was chosen, and two of its assumptions
did not survive contact with the current crate ecosystem.

| Design said | Actual | Consequence |
|---|---|---|
| `fastembed` 5.16.2, `InitOptions`, `embed(&str)` | 6.0.0, `TextInitOptions`, `embed(Vec<&str>, Option<usize>)` | The embedder wraps a batch-only API. A single-text `embed` is a convenience over the batch call, not the primitive. |
| `embed` takes `&self` | takes `&mut self` | `Embed` is shared behind an `Arc`, so the model sits behind a `Mutex`. No real cost: ONNX inference is CPU-bound and already internally parallel. |
| One `to_pgvector` signature serves both builds | The non-semantic build must still bind a *typed* NULL | `Option::<Vec<f32>>::None` binds as `real[]`, and `COALESCE(real[], vector)` is an error that broke **every** `update` on Postgres in a default build — not only ones involving embeddings. Fixed with `$n::vector` casts and a regression test that is deliberately not feature-gated. |
| SQLite gets `BLOB` + cosine in Rust | Not in scope | `SqliteStore::vector_search` returns `Unsupported`. SQLite stays keyword-only. |
| Both backends satisfy a semantic contract | Only Postgres implements it | The contract suite cannot assert the two agree, because they no longer do. |

The third row is a real cost, accepted deliberately: SQLite is the default
backend, so semantic search is unavailable unless the user opts into Postgres.
The `Unsupported` error must say that in words, because a silent empty result
would look like "no memories matched" rather than "this backend cannot do that".

## Deployment

Compose runs Postgres with pgvector. The mem8 binary stays local and is spawned
by the agent over stdio; it reaches the database over TCP. Running mem8 itself
in a container needs an HTTP transport, which does not exist yet and is not part
of this plan.

```
[agent] --stdio--> [mem8, local] --postgres://--> [pgvector, docker]
```

## Phase 1 — Postgres gains a vector column

### 1.1 Compose file

`docker-compose.yml` at the repo root, `pgvector/pgvector:pg16`. Publishes 5432,
names the database `mem8`, and persists to a named volume so memories survive
`docker compose down`.

The password is a development default and the file must say so. This listens on
localhost; exposing it to a network needs a real secret, and the README says
that rather than leaving the reader to infer it.

### 1.2 Schema version guard for Postgres

`SqliteStore` refuses a database whose `user_version` exceeds what the binary
knows. `PgStore` has no equivalent, so an older binary pointed at a newer
database silently misreads it. Adding a vector column is the project's first
real Postgres migration, so the guard lands here.

A `mem8_meta` table holds one row: `schema_version INT`. On connect:

- Table absent → this is a fresh or pre-versioning database. Create it, run the
  migration, record the current version.
- Version > `PG_SCHEMA_VERSION` → `Mem8Error::Migration`, refuse to open.
- Version < `PG_SCHEMA_VERSION` → migrate forward, then update the row.

Two mem8 processes may connect at once, so this runs inside a transaction
holding a lock on `mem8_meta`. Without it, both see "no table", both migrate,
and one fails on a duplicate column.

### 1.3 The migration

```sql
CREATE EXTENSION IF NOT EXISTS vector;
ALTER TABLE memories DROP COLUMN IF EXISTS embedding;   -- BYTEA placeholder
ALTER TABLE memories ADD COLUMN embedding vector(384);
CREATE INDEX IF NOT EXISTS idx_embedding
    ON memories USING hnsw (embedding vector_cosine_ops);
```

The existing `embedding BYTEA` column is always NULL — nothing ever wrote it —
so dropping it loses no data. Verify that claim against a real database before
running the drop, not after.

`CREATE EXTENSION` needs privileges an unprivileged user may lack. If it fails,
the error must name the extension and say the database owner has to install it,
rather than surfacing as a bare SQL error.

**Deliverables:** compose file, `PgStore` version guard + migration, tests for
all three guard branches, README section on running Postgres.

## Phase 2 — Embeddings

### 2.1 The `embed` module, feature-gated

```toml
[features]
semantic = ["dep:fastembed", "dep:pgvector"]
```

```rust
// src/embed/mod.rs
pub const EMBEDDING_DIM: usize = 384;

pub trait Embed: Send + Sync {
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_one(&self, text: &str) -> Result<Vec<f32>>;   // batch of one
}

pub struct Embedder { /* fastembed::TextEmbedding */ }
```

A trait, not a bare struct, because tests must not download 130 MB. `Embedder`
is the real implementation; tests substitute a deterministic fake.

The dimension is asserted against the model at load time. A model change that
silently altered the dimension would corrupt every subsequent write, and the
`vector(384)` column would reject it at insert with an error naming neither
cause.

### 2.2 `Store` gains `vector_search`

```rust
async fn vector_search(&self, query: VectorQuery) -> Result<Vec<SearchHit>>;
```

`VectorQuery` mirrors `SearchQuery` but carries `embedding: Vec<f32>` instead of
`text`. Filters (project, kind, tags, limit) apply identically — a semantic
search that ignored project scope would leak across projects.

- `PgStore` — `ORDER BY embedding <=> $1` (cosine distance), `WHERE embedding IS NOT NULL`.
- `SqliteStore` — `Err(Mem8Error::Unsupported)`, naming the backend and pointing at Postgres.
- `MemStore` — real cosine in Rust, so `core` tests exercise merging without a database.

New error variant: `Unsupported { feature: String, backend: String }`.

### 2.3 Writes embed

`Memory8` holds `Option<Arc<dyn Embed>>`. When present, `add` and `update`
compute an embedding and pass it down.

Embedding failure must not lose the memory. A model that fails to load, or an
embed call that errors, degrades to a keyword-only write — the memory is stored
with a NULL embedding and remains findable by keyword. Losing a write because a
similarity index was unavailable is a worse outcome than a memory that is
temporarily invisible to semantic search.

`Store::add`/`update` take the embedding through `NewMemory`/`MemoryUpdate`.

### 2.4 `mem8 reindex`

Existing rows have no embedding and are invisible to vector search until
backfilled. `mem8 reindex` embeds every memory where `embedding IS NULL`, in
batches, reporting progress. It is the only path by which an existing database
gains semantic search.

Idempotent: running it twice does nothing the second time. `--all` re-embeds
everything, for a model change.

### 2.5 RRF merge in `core`

Keyword and vector scores are not comparable — BM25, `ts_rank`, and cosine
distance are three different scales. Ranks are comparable.

```
score(m) = Σ  1 / (60 + rank_in_list)
```

Both searches run, results merge by id, sort by fused score, truncate to limit.

Degradation is one-directional: if vector search fails or is unsupported,
keyword results stand alone and search still works. This is what keeps SQLite
users unaffected by everything in phase 2.

## Testing

| Test | Proves |
|---|---|
| Fake embedder, deterministic vectors | Merge logic without a 130 MB download |
| RRF: found-by-both outranks found-by-one | The fusion does what it claims |
| `SqliteStore::vector_search` → `Unsupported` | Naming the backend, not an empty result |
| Vector search respects project scope | No cross-project leak |
| Keyword-only path unchanged with feature off | Existing users unaffected |
| `reindex` twice is a no-op | Idempotence |
| Embed failure still stores the memory | Degradation, not data loss |
| Guard: newer DB refused, older migrated, fresh initialised | All three branches |
| **Real model**: "we picked X" finds "we chose X" | The reason this exists. Ignored by default; opt in with `MEM8_TEST_EMBED=1` |
| **Real model**: `SqliteStore` outranks `PgStore` for query `SqliteStore` | Exact identifiers do not regress |

The last two need the real model, so they are opt-in and excluded from CI, in
the same way `MEM8_TEST_PG` gates the Postgres contract.

## Order

1. Compose + version guard + migration (phase 1, no embedding yet)
2. `embed` module behind the feature, provable in isolation
3. `vector_search` on the three stores + tests
4. Embedding on write + `reindex`
5. RRF merge in `core`
6. Docs

Each step leaves the tree green with the feature both on and off. `cargo test`
with no feature and no database must pass throughout — that is the existing
user's experience, and it is not allowed to regress.

## Measured behaviour

Cosine similarity against BGE-small, on the memory *"We chose the porter
tokenizer so both backends stem identically."*

| Query | Cosine |
|---|---|
| "why did we pick the porter tokenizer" (reworded) | 0.806 |
| "why did we pick that word splitter" (no shared keyword) | 0.604 |
| "Run cargo fmt before every commit." (unrelated) | 0.416 |
| "kubernetes ingress annotations" (unrelated) | 0.479 |

And for the identifier case, query `SqliteStore`: 0.814 against the
`SqliteStore` memory, 0.580 against the near-identically-worded `PgStore` one.

Two things follow. The separation is wide enough that hybrid retrieval works —
even the hardest case sits well clear of the unrelated pairs. But unrelated text
still scores 0.42–0.48, not near zero, so **cosine here is only meaningful
relatively**. Any future absolute threshold, semantic duplicate detection in
particular, has to be calibrated against real data rather than set to a round
number.

## What this does not do

- SQLite semantic search. Keyword-only, by decision.
- Remote mem8. Needs HTTP transport, auth, TLS, and explicit project scoping.
- Semantic duplicate detection. `find_duplicate` still uses word overlap; the
  0.8 threshold does not transfer to cosine and needs its own measurement.
