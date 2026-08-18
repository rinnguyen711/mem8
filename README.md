# mem8

Persistent memory for AI coding agents, served over the Model Context Protocol.

Memories are stored in SQLite by default and scoped to the project you are
working in, so the agent recalls what it learned in earlier sessions without
carrying every project's notes into every conversation.

## Install

You need [Rust](https://rustup.rs) and [Claude Code](https://claude.com/claude-code).
There are no prebuilt binaries yet, so mem8 is compiled from source.

```bash
git clone https://github.com/rinnguyen711/mem8
cd mem8

# 1. Build and install the binary
cargo install --path .

# 2. Install the plugin: registers the MCP server and points the agent at it
claude plugin marketplace add ./
claude plugin install mem8@mem8
```

Restart Claude Code afterwards — MCP tools are registered when a session starts.

The order matters. The plugin runs the `mem8` binary, so installing the plugin
first leaves it pointing at something that is not there yet.

Verify:

```bash
mem8 --version               # 0.1.0
claude mcp list | grep mem8  # mem8: mem8 serve - ✔ Connected
```

Then ask the agent to remember something. Memories go to `~/.mem8/mem8.db`,
scoped automatically to the git repository you are working in.

### Without the plugin

The plugin is only a convenience: it declares the MCP server and tells the agent
to prefer mem8 over Claude Code's own file-based memory. To wire the server up by
hand instead:

```bash
claude mcp add --scope user mem8 -- mem8 serve
```

Or add it to your MCP configuration directly:

```json
{
  "mcpServers": {
    "mem8": {
      "command": "mem8",
      "args": ["serve"]
    }
  }
}
```

### Updating

```bash
cargo install --path . --force
```

On Windows the running server holds a lock on `mem8.exe`, so stop it first with
`taskkill /IM mem8.exe /F`.

## Tools

| Tool | Purpose |
|---|---|
| `add_memory` | Store a decision, preference, convention, fact, or learning. |
| `search_memory` | Search, scoped to the current project by default. Keyword, plus semantic when [enabled](#semantic-search). |
| `get_memory` | Retrieve one memory in full by id. |
| `update_memory` | Revise a memory rather than storing a contradictory one. |
| `delete_memory` | Remove a memory permanently. |

`kind` is a fixed enum: `decision`, `preference`, `convention`, `fact`, or
`learning`. Sending anything else is rejected before the tool runs, as a
protocol-level error naming all five valid values — not as a normal tool
result.

Search results are ordered best-first and carry a relevance score, so a strong
match is distinguishable from the long tail beneath it:

```
[2026-08-16] (decision, score 1.163) mem8 ships as a Claude Code plugin...
[2026-08-16] (decision, score 0.812) Removed the mem0 plugin and marketplace...
```

The scale differs by backend — SQLite reports a negated BM25 score, Postgres a
`ts_rank` value — so compare scores within one result set, not across backends.

Storing content that is near-identical to an existing memory in the same project
revises that memory instead of creating a second copy. The comparison is word
overlap, so it catches the same text saved twice but not the same idea worded
differently — that needs semantic similarity, which v1 does not have.

Searches that return nothing are appended to `~/.mem8/missed-searches.log` with
the query and its sanitized form, which is what shows whether a miss was a
synonym problem or genuinely absent content. It stays on your machine; set
`MEM8_NO_MISS_LOG` to turn it off.

Project scope is detected automatically from the git root of the working
directory (its directory name), falling back to the working directory's own
name if there is no `.git`. Pass `project` to override it, or `global: true`
on `search_memory` to search across every project.

## Storage

`MEM8_DB` selects the backend:

- unset — SQLite at `~/.mem8/mem8.db`
- `sqlite://path/to/file.db` — SQLite at that path (relative or absolute; an
  absolute path produces the familiar three-slash form, e.g.
  `sqlite:///home/me/mem8.db`)
- `postgres://user@host/db` or `postgresql://user@host/db` — Postgres

Both backends satisfy the same contract suite, run against a real server. SQLite
is the default and has seen far more use, so treat Postgres as the newer of the
two.

## Semantic search

Off by default. With it on, a question worded differently from the memory still
finds it — "why did we pick the porter tokenizer" finds a memory recorded as "we
**chose** the porter tokenizer", which keyword search cannot do.

It needs **Postgres with pgvector**; SQLite stays keyword-only.

```bash
# 1. Start Postgres with pgvector
docker compose up -d

# 2. Build with the feature and point mem8 at it
cargo install --path . --features semantic --force
export MEM8_DB=postgres://mem8:mem8@localhost:5432/mem8

# 3. Embed the memories you already have
mem8 reindex
```

Step 3 matters: memories written before this was enabled have no embedding and
stay invisible to semantic search until backfilled. `mem8 reindex` is safe to
re-run — it only touches memories that lack one — and is also what you run after
`mem8 import`.

Both searches run on every query and their results are merged by rank
(Reciprocal Rank Fusion). Keyword search is not replaced, because exact
identifiers — `SqliteStore`, `auth-token`, a commit hash — are what an agent's
memory is full of and are exactly what embeddings match badly. A memory found by
both searches ranks above one found by either alone.

What it costs:

- **A bigger binary.** Roughly 10 MB to over 100 MB; fastembed bundles an ONNX
  runtime.
- **A one-time download.** ~130 MB for BGE-small-en-v1.5, to `./.fastembed_cache`.
  Offline after that — no API key, no request leaves your machine.
- **Slower writes.** Every `add_memory` computes an embedding, tens of
  milliseconds on CPU.

Degradation is deliberate throughout. If the model fails to load, mem8 runs
keyword-only and says so. If embedding a particular memory fails, the memory is
still stored — findable by keyword, and backfillable later by `mem8 reindex`.
Losing a write because an index was unavailable would be the worse outcome.

On SQLite, or in a build without the feature, everything above is simply absent
and search behaves exactly as it always has.

## Running mem8 as a shared server

Off by default. The ordinary shape is stdio — the agent runs mem8 as a child
process and the memory is yours alone, which needs no server, no token, and no
certificate. This section is for the other shape: one mem8, several clients,
reached over a network.

```bash
export MEM8_TOKEN=$(openssl rand -hex 32)
docker compose --profile server up -d

claude mcp add --transport http mem8 http://127.0.0.1:8080/mcp \
  --header "Authorization: Bearer $MEM8_TOKEN"
```

`--profile server` is deliberate: a plain `docker compose up` still starts only
the database, so running Postgres locally never publishes a server by accident.

### Every call must name its project

Locally, mem8 infers the project from the working directory. A server cannot —
its working directory is `/app`, the same for every client — so in HTTP mode
`project` is **required** and a call without it is refused:

```
'project' is required when mem8 serves over HTTP: the server cannot infer
which project you mean, because its working directory is its own rather
than yours.
```

Refusing is the point. The alternative is filing every client's memories into
one shared scope, which nothing would report.

`global: true` still works on `search_memory`, since it is explicit about
crossing projects.

### Authentication and TLS

A bearer token from `MEM8_TOKEN`, compared in constant time. mem8 refuses to
start over HTTP without one — an unauthenticated memory server is readable by
anyone who finds the port, so failing to start is the safer default.

TLS is required for any bind that is not loopback:

```bash
mem8 serve --http 0.0.0.0:8080                             # refuses to start
mem8 serve --http 0.0.0.0:8080 --tls-cert c.pem --tls-key k.pem   # ok
mem8 serve --http 127.0.0.1:8080                           # ok, proxy in front
mem8 serve --http 0.0.0.0:8080 --insecure                  # ok, and unwise
```

A bearer token on a plaintext connection is readable by anything on the path,
and a captured token is complete access to every memory. The compose file binds
`127.0.0.1:8080` and expects a TLS-terminating proxy in front.

### What this does not protect against

Worth reading before exposing it to anything:

- **One token, one trust level.** Every caller who authenticates can read and
  write every project. There is no per-user isolation.
- **No rate limiting and no audit log.** A stolen token is unlimited, and leaves
  no trace beyond the memories it changes.
- **No rotation.** Changing the token means restarting the server.

This is built for a private network or a VPN, with TLS and a real secret. It is
not built to face the public internet.

## Backup

```bash
mem8 export memories.md
mem8 import memories.md
```

Import always creates new memories; it never overwrites or deduplicates
against existing ones, so importing the same file twice doubles the entries.

The markdown format carries no embeddings, so run `mem8 reindex` after an
import if you use semantic search.

### Markdown format

Each memory is a section starting with `## ` followed by a UUID. Only that
exact shape starts a new section — a content line that happens to begin with
`## ` (e.g. a markdown heading you wrote as part of a note) is left alone and
stays part of the current memory's body, so round-tripping arbitrary content
is safe.

Within a section, `- project:`, `- kind:`, and a non-empty body are all
required; missing any of them fails the import with an error naming the
offending section. A malformed or missing heading is different: it produces no
error, because nothing is recognised as a section — that text is simply not
imported. `- created:` is informational only. Tags are written
as a JSON array (`- tags: ["a,b","c"]`); a legacy comma-separated form is
still accepted on import but can't unambiguously represent a tag containing a
comma.

Real output of `mem8 export` after importing two memories:

```markdown
# mem8 export

## f7909ab9-df1b-4792-8ca5-b7888e4129b9
- project: mem8
- kind: decision
- tags: ["storage","sqlite"]
- created: 2026-08-12T03:01:11.083328700+00:00

We store memories in SQLite by default because it needs no server and
ships inside the binary via the bundled rusqlite feature.

This keeps the zero-config path working for a single developer on a
laptop, which is the common case.

## 46913455-4d32-4e96-9edd-48cd5d1a2ad7
- project: mem8
- kind: convention
- tags: []
- created: 2026-08-12T03:01:11.196515700+00:00

Run `cargo fmt` before every commit.
```

## Status

Version 0.1.0. It works, and it is used daily by its author — but it is young,
and these are the things worth knowing before you rely on it.

**Search is keyword-only by default.** Every word in a query must appear in a
memory for it to match, so a memory recorded as "we chose the porter tokenizer"
is not found by "why did we pick the porter tokenizer" — `chose` and `pick` are
different words. Search with two or three distinctive keywords and try different
words if nothing comes back. [Semantic search](#semantic-search) fixes this, but
needs Postgres and a larger binary.

**Semantic search is new and Postgres-only.** SQLite — the default — cannot do
it. It has been verified against a real pgvector database and the real
embedding model, but it has not yet been used daily the way keyword search
has.

**Only tested on Windows.** The code has no platform-specific logic and the test
suite should pass anywhere, but nobody has run it on macOS or Linux yet.

**Postgres is newer than SQLite.** Both satisfy the same contract suite, verified
against a real server, but SQLite is the default and has had far more use.

## Development

```bash
cargo test                                              # SQLite only
MEM8_TEST_PG=postgres://localhost/mem8_test cargo test  # includes Postgres

# Vector search, against a real pgvector database
docker compose up -d
MEM8_TEST_PG=postgres://mem8:mem8@localhost:5432/mem8 \
  cargo test --features semantic --test pg_vector

# The real embedding model. Downloads ~130 MB on first run, so it is opt-in
# twice and never runs in CI.
MEM8_TEST_EMBED=1 cargo test --features semantic --test real_model
```

The test suite covers the storage backends against a shared contract, the core
service, the MCP tool surface, an end-to-end handshake against the real binary
over stdio, an export/import round trip, the Postgres schema-version guard
(including two processes migrating at once), and vector search against a real
pgvector database.

`real_model` is the one suite that exercises the actual embedding model. It
asserts the claim the feature rests on: that a reworded question lands closer to
a memory than an unrelated sentence does, and that an exact identifier is not
displaced by a semantically similar memory.

`docs/superpowers/` holds the design spec and the implementation plan the project
was built from, if you want the reasoning behind a decision.

## License

MIT. See [LICENSE](LICENSE).
