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
| `search_memory` | Keyword search, scoped to the current project by default. |
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

## Backup

```bash
mem8 export memories.md
mem8 import memories.md
```

Import always creates new memories; it never overwrites or deduplicates
against existing ones, so importing the same file twice doubles the entries.

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

**Search is keyword-only.** Every word in a query must appear in a memory for it
to match, so a memory recorded as "we chose the porter tokenizer" is not found by
"why did we pick the porter tokenizer" — `chose` and `pick` are different words.
Search with two or three distinctive keywords and try different words if nothing
comes back. Semantic search is the obvious next step; the schema reserves an
`embedding` column for it.

**Only tested on Windows.** The code has no platform-specific logic and the test
suite should pass anywhere, but nobody has run it on macOS or Linux yet.

**Postgres is newer than SQLite.** Both satisfy the same contract suite, verified
against a real server, but SQLite is the default and has had far more use.

## Development

```bash
cargo test                                              # SQLite only
MEM8_TEST_PG=postgres://localhost/mem8_test cargo test  # includes Postgres
```

The test suite covers the storage backends against a shared contract, the core
service, the MCP tool surface, an end-to-end handshake against the real binary
over stdio, and an export/import round trip.

`docs/superpowers/` holds the design spec and the implementation plan the project
was built from, if you want the reasoning behind a decision.

## License

MIT. See [LICENSE](LICENSE).
