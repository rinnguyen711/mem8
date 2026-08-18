# Fact supersession, temporal queries, semantic dedup, and recency decay

Status: design, approved 2026-08-17.

## Why

mem8 stores every memory as a live fact forever. Nothing records that one fact
replaced another, so a project that changes its mind accumulates contradictions
and returns them all:

```
[2026-08-12] (decision, score 1.163) We store memories in SQLite by default
[2026-08-17] (decision, score 0.984) Default backend is now Postgres
```

Both are returned, ranked by text relevance rather than by which one is true.
The agent picks whichever scored higher.

Existing duplicate detection does not help. `find_duplicate` compares by word
overlap at a threshold of 0.8, and the comment on `DUPLICATE_THRESHOLD` records
the measurement that matters: two memories both recording the choice of the
porter tokenizer scored 0.14, indistinguishable from unrelated text. Word
overlap catches a literal re-save. It cannot catch a change of mind.

This design adds the smallest mechanism that fixes it — a fact can be marked
superseded by another — plus three changes that follow naturally once memories
carry a validity window.

Prior art is Zep, whose temporal knowledge graph gives every edge a validity
window and invalidates superseded facts while keeping them queryable. The
mechanism copied here is the validity window alone. The graph, the entity
extraction, and the LLM call per write are deliberately not copied; see
"Rejected alternatives".

## Scope

In:

1. **Supersession.** `invalid_at` / `superseded_by` columns, an explicit
   `supersedes` parameter on `add_memory`, and search that hides superseded
   memories by default.
2. **Semantic dedup.** Cosine similarity as a second duplicate signal, behind
   the existing `semantic` feature.
3. **Recency decay.** Optional score decay by age, off unless configured.
4. **Temporal queries.** `as_of` on search, answering what was believed at a
   past time.

Out: entity extraction, graph traversal, multi-hop retrieval, per-user tenancy.

## 1. Data model and schema

Two nullable columns on `memories`:

```sql
superseded_by TEXT       -- uuid of the memory that replaced this one
invalid_at    TIMESTAMP  -- when it stopped being true
```

Both NULL means live. They are set together and never independently: one is the
pointer, the other the time, and a row carrying only one is incoherent. This is
enforced in the store layer rather than by a database constraint, so both
backends behave identically and the rule lives next to the code that applies it.

`Memory` gains:

```rust
pub superseded_by: Option<Uuid>,
pub invalid_at: Option<DateTime<Utc>>,
```

Both marked `#[serde(skip_serializing_if = "Option::is_none")]`, matching
`embedding`. A live memory therefore serializes exactly as it does today, and no
existing consumer of the tool output sees a new field.

### Migration

Both backends check a stored schema version and refuse a database from the
future. Neither currently alters an existing table — SQLite's `init` runs
`CREATE TABLE IF NOT EXISTS` and then bumps `user_version`; Postgres's migration
does the equivalent. Bumping the constant alone would stamp a v1 database as v2
while leaving it without the new columns, and every later query would fail on a
missing column.

So each backend needs a real migration step:

- SQLite: `SCHEMA_VERSION` 1 to 2. When the found version is 1, run
  `ALTER TABLE memories ADD COLUMN` for each new column before updating
  `user_version`. Both statements and the version bump run in one transaction,
  so an interrupted upgrade leaves a v1 database rather than a half-migrated one.
- Postgres: `PG_SCHEMA_VERSION` 2 to 3, same shape, inside the existing advisory
  lock that already serializes concurrent migrations.

Existing rows migrate to NULL/NULL. Every memory that exists today stays live
and keeps being returned. Nothing disappears on upgrade.

## 2. Search filtering

`SearchQuery` and `VectorQuery` each gain:

```rust
pub include_superseded: bool,       // default false
pub as_of: Option<DateTime<Utc>>,   // default None
```

Both query types, not only the keyword one. A semantic search that ignored
supersession would surface dead facts that keyword search correctly hides — the
same reasoning already applied to `project` on `VectorQuery`, whose doc comment
notes that filters are not optional extras.

Three modes, one predicate:

| Mode | Predicate |
|---|---|
| default | `invalid_at IS NULL` |
| `include_superseded: true` | none |
| `as_of: T` | `created_at <= T AND (invalid_at IS NULL OR invalid_at > T)` |

`as_of` and `include_superseded` are mutually exclusive. `as_of` already
specifies exactly which rows count, so combining them is a contradiction rather
than a refinement. A call setting both is rejected at the tool boundary with an
error naming both parameters, not silently resolved in favour of one.

`get_memory` never filters. A superseded memory is still retrievable in full by
id: it is hidden from discovery, not deleted. That distinction is the whole
reason this is not `delete_memory`.

`find_duplicate` gains the default filter, so a new write never merges into a
memory that is already dead.

## 3. Write path

`add_memory` gains an optional parameter:

```rust
supersedes: Option<Uuid>
```

The agent has just searched before writing, so it already knows which memory it
is replacing. Asking it to say so costs one field. Inferring the same fact
costs an LLM call per write and can be wrong.

### Ordering

The new memory is written first, then the old one is invalidated pointing at it.
The reverse order would leave a window in which the old fact is dead and nothing
has replaced it; a crash inside that window loses the fact entirely. In the
chosen order, a crash after the first step leaves two live memories — the
condition that exists today, and which the next write can still repair.

### Validation

Before either write, and returning without writing anything if any check fails:

- the target exists, else `NotFound`
- the target is in the same project, else `InvalidInput` — a cross-project
  supersession is far more likely a mistaken id than an intent
- the target is not already superseded, else `InvalidInput` naming the existing
  `superseded_by`, so chains stay linear and a memory has at most one successor

### Store trait

```rust
async fn supersede(&self, old: Uuid, new: Uuid, at: DateTime<Utc>) -> Result<()>;
```

Implemented by both backends and by `MemStore`, whose filtering must match, or
the core unit tests drift from what the real backends do.

### Interaction with dedup

When `supersedes` is passed explicitly, `find_duplicate` is skipped entirely.
The agent has already stated what this memory replaces; re-deriving it by word
overlap can only disagree with an answer we were given directly.

## 4. Semantic dedup

`find_duplicate` gains a second signal under `#[cfg(feature = "semantic")]`,
active only when the embedder loaded.

A candidate is a duplicate if word overlap is at least `DUPLICATE_THRESHOLD`
(today's rule, unchanged) **or** cosine similarity is at least the new
threshold and the candidate has the same `kind`. The same-kind guard matters
because a `decision` and a `learning` about one subject are legitimately
different memories that will sit close together in embedding space.

This is decoupled from supersession. With explicit-only triggering, semantic
similarity never invalidates anything; it only catches re-saves that word
overlap misses — the porter-tokenizer case that scored 0.14.

The starting threshold of 0.9 is a guess, not a measurement. It must be
calibrated against real memories before the default lands, using the shape the
`real_model` suite already establishes: a reworded memory must score above the
threshold while a merely related memory scores below. If no threshold separates
those cleanly, this item ships disabled rather than shipping a number that
silently merges distinct memories, since merging discards the older content.

With the feature off, or the embedder unavailable, behaviour is identical to
today. This matches the degradation contract already stated for semantic search.

## 5. Recency decay

Read `MEM8_RECENCY_HALFLIFE_DAYS`. Unset means today's behaviour exactly.

```rust
score *= 0.5_f64.powf(age_days / halflife)
```

Off by default because it changes the ranking of every existing search result,
and because mem8's convention for unproven behaviour is an opt-in switch —
`MEM8_DB`, `MEM8_NO_MISS_LOG`, and the `semantic` feature all work this way.

Applied in `core`, on hits returned by the store, not in SQL. Two reasons. The
score scale already differs by backend — SQLite reports a negated BM25 value,
Postgres a `ts_rank` — so decay applied in SQL would compound that divergence
into a third scale. And with semantic search enabled, results are merged by
Reciprocal Rank Fusion, so decay must apply to the fused ranking rather than to
each input list separately.

Age is measured from `updated_at`, not `created_at`: revising a memory is
evidence it is still live.

An invalid value — zero, negative, or unparseable — logs a warning and disables
decay rather than failing the search. This follows `log_missed_search`, which
already establishes that observability must never break the operation it
observes.

## 6. Export and import

The markdown format gains two optional fields:

```
- superseded_by: <uuid>
- invalid_at: <rfc3339>
```

Written only when set, so a live memory exports byte-identically to today.

Both are optional on import, so existing export files still load. This matters
more than it appears: without round-tripping these fields, `mem8 export`
followed by `mem8 import` silently resurrects every dead fact, and the backup
path becomes the way contradictions come back.

Import already creates new memories rather than merging, and `superseded_by`
holds a uuid that import does not preserve. So import must remap: memories are
loaded first, then `superseded_by` values are rewritten to the new ids using the
old-to-new mapping built during load. A `superseded_by` pointing at a uuid not
present in the file is dropped with a warning, leaving `invalid_at` set — the
memory is still known to be dead, only its successor is unknown.

## 7. Testing

The contract suite runs against both real backends, so supersession semantics
are verified once and hold for SQLite and Postgres alike:

- supersede hides the old memory from search; `get` still returns it in full
- `include_superseded: true` returns both
- `as_of` before, exactly at, and after the invalidation boundary
- rejection cases: already-superseded target, cross-project target, missing target
- the new memory survives when invalidation fails, per the ordering guarantee

Migration tests extend the existing Postgres suite: v2 to v3 preserves every row
as live, and the concurrent-migration case already covered there must still pass
with the new step. SQLite gets the equivalent: open a v1 database, reopen it,
confirm the columns exist and existing rows read back live.

Decay and dedup are core unit tests against `MemStore` — pure ranking and
comparison logic needing no backend. `MemStore` must gain the same filtering as
the real backends so it does not drift.

An export/import round trip asserts that a superseded memory returns superseded,
with its successor pointer remapped to the newly imported id.

## Rejected alternatives

**Auto-detecting supersession by similarity.** Considered and rejected for v1.
A false positive silently hides a live memory, which is worse than the problem
being solved, and the 0.14 measurement shows lexical overlap cannot carry the
signal. Revisit only if semantic dedup produces a threshold that separates
cleanly on real data.

**Using `update_memory` instead.** Update overwrites content, so the old fact is
gone. Supersession keeps "we used SQLite until 2026-08-17" answerable, which is
the property that makes past decisions explicable.

**Entity extraction and graph traversal.** These are what Zep does beyond the
validity window, and they need an LLM call per write plus a graph store. That
trades away the two properties mem8 is built on: writes are free, and nothing
leaves the machine. A different product, not a bigger version of this one.

## Sequencing

Items are independently shippable and in dependency order:

1. Schema, migrations, model fields — everything else needs the columns.
2. Supersede write path and default search filtering — the feature proper.
3. `as_of` — a second predicate over the same columns.
4. Export/import round-tripping — must land before anyone relies on backups.
5. Semantic dedup — independent, gated on threshold calibration.
6. Recency decay — independent, off by default.

Items 1 through 4 are the coherent unit. 5 and 6 can slip without leaving
anything half-built.

## Documentation

The README needs: the `supersedes` parameter in the tool table, a section on
supersession and `as_of`, the new markdown fields in the format description, and
`MEM8_RECENCY_HALFLIFE_DAYS` alongside the other environment variables.

The change to `search_memory`'s default result set is a behaviour change to the
tool contract even though no existing memory is affected, so it belongs in the
README's own words rather than only in a commit message.
