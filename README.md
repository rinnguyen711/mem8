# mem8

Persistent memory for AI coding agents, served over the Model Context Protocol.

Memories are stored in SQLite by default and scoped to the project you are
working in, so the agent recalls what it learned in earlier sessions without
carrying every project's notes into every conversation.

## Install

```bash
cargo install --path .
```

## Use with Claude Code

Add to your MCP configuration:

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

## Tools

| Tool | Purpose |
|---|---|
| `add_memory` | Store a decision, preference, convention, fact, or learning. |
| `search_memory` | Keyword search, scoped to the current project by default. |
| `get_memory` | Retrieve one memory in full by id. |
| `update_memory` | Revise a memory rather than storing a contradictory one. |
| `delete_memory` | Remove a memory permanently. |

`kind` is a fixed enum: `decision`, `preference`, `convention`, `fact`, or
`learning`. Sending anything else fails with an error naming all five valid
values.

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
- `postgres://user@host/db` — Postgres

Postgres support is compile-verified only: the code builds and the contract
tests are written against it, but it has not been exercised against a live
Postgres server during development. Treat it as experimental until you've
tried it against your own database.

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

A section requires a UUID heading, a `- project:` line, a `- kind:` line, and
a non-empty body; missing any of them fails the import with an error naming
the offending section. `- created:` is informational only. Tags are written
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

## Development

```bash
cargo test                                              # SQLite only
MEM8_TEST_PG=postgres://localhost/mem8_test cargo test  # includes Postgres
```
