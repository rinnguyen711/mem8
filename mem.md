# mem8 export

## 1a9044a0-aafa-4370-b9c6-074cad62911f
- project: mem8
- kind: fact
- tags: ["mem8","setup"]
- created: 2026-08-12T06:21:40.153219800+00:00

mem8 stores memories in ~/.mem8/mem8.db by default, scoped per git repo.

## d9e19346-e199-4d20-b4ac-961b5a1ad4d6
- project: mem8
- kind: preference
- tags: ["style"]
- created: 2026-08-12T06:50:32.564223+00:00

Rin prefers caveman mode for all Claude Code sessions.

## cfdb2b05-bd26-4511-8273-e27b7ace8d42
- project: mem8
- kind: decision
- tags: ["search","sqlite"]
- created: 2026-08-12T06:50:32.564251900+00:00

mem8 uses SQLite FTS5 with the porter tokenizer so it stems like Postgres does.

## d9c92302-ea8b-4622-8904-3cbd383ed6cd
- project: mem8
- kind: convention
- tags: ["cli"]
- created: 2026-08-12T06:50:50.242448600+00:00

Pass --dry-run to preview changes without writing them.

## 7b1940e6-4f8d-49ff-9ca7-ffe8f15118be
- project: otherproj
- kind: convention
- tags: []
- created: 2026-08-12T06:51:06.779048300+00:00

This project uses tabs, not spaces.

## f8d27815-82a4-4e28-a05f-235003eb3a05
- project: mem8
- kind: decision
- tags: ["search","tokenizer","sqlite","backends"]
- created: 2026-08-12T06:53:20.730767+00:00

We chose the Porter tokenizer for search so both backends stem identically and return the same results for a given query.

