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

## 07e4f150-a93c-41e0-8c42-9339b5759818
- project: mem8
- kind: decision
- tags: ["plugin","packaging","install"]
- created: 2026-08-16T17:24:02.174639300+00:00

mem8 ships as a Claude Code plugin. The plugin lives in the plugin/ subdirectory with marketplace.json at the repo root. Install is two steps: cargo install --path . for the binary, then claude plugin install mem8@mem8 for the MCP config and session-start hook. Packaging it in its own subdirectory matters: sourcing the plugin from the repo root copied the entire tree including target/, which was 4.5 GB instead of 9 KB.

## 539f2568-26b6-45cf-bc31-5364f76d6ead
- project: mem8
- kind: decision
- tags: ["mem0","cleanup","plugins"]
- created: 2026-08-16T17:27:15.307442800+00:00

Removed the mem0 plugin and marketplace. Its MCP server never worked (HTTP 401, no valid API key), its hooks had unquoted ${CLAUDE_PLUGIN_ROOT} that broke on paths containing spaces, and its SessionStart banner printed "Mem0 Active" while instructing the agent to display a false status line. mem8 covers the same job. Three inert references remain in ~/.claude.json (a disabledMcpServers entry and two usage counters) and were deliberately left alone.

## a39450f9-a332-4e81-8883-e3c718ca5476
- project: mem8
- kind: fact
- tags: ["github","publishing","repo"]
- created: 2026-08-16T17:35:29.471345700+00:00

mem8 is published at https://github.com/rinnguyen711/mem8 on the main branch. The push worked through the Windows Git Credential Manager; the gh CLI is not authenticated on this machine. mem.md was untracked before publishing because it was a memory export from testing that contained a personal preference.

## 3f6501aa-afcb-4ddb-b6e0-ed15e3ef6dbf
- project: mem8
- kind: learning
- tags: ["plugin","hooks","prompting"]
- created: 2026-08-16T17:53:33.449715700+00:00

The mem8 plugin needs two hooks, not one. SessionStart alone fails: its instruction is buried under tool output within a few turns, and the original wording was conditional ("search before concluding something is unknown"), which only fires when the agent is about to admit ignorance rather than when it can answer from the code. The fix was unconditional wording plus a UserPromptSubmit hook restating the rule each turn, which is the same structure the caveman plugin uses to survive a long session.

