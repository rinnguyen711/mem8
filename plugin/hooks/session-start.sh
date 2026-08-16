#!/usr/bin/env bash
# Tell the agent that mem8 owns memory in this session.
#
# Claude Code's own system prompt describes a file-based memory directory, and a
# tool description alone does not outrank it — in practice memories were written
# to files while mem8 sat unused. This hook states the preference directly, which
# does outrank it. The companion `autoMemoryEnabled: false` setting turns the file
# directory off entirely; this hook is what points the agent at mem8 instead.
#
# Emits the JSON envelope Claude Code reads from a SessionStart hook. Any failure
# must stay silent: a memory tool that breaks sessions is worse than one that is
# occasionally forgotten.

set -uo pipefail

emit() {
  # $1: additionalContext string (already JSON-escaped)
  printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"%s"}}\n' "$1"
}

if ! command -v mem8 >/dev/null 2>&1; then
  emit "The mem8 plugin is installed but the \`mem8\` binary is not on PATH, so its memory tools are unavailable. Install it with \`cargo install --path .\` from the mem8 repository, then restart. Do not fall back to writing memory files."
  exit 0
fi

emit "Memory for this session is provided by mem8, through its MCP tools. Call \`search_memory\` at the start of any task or question that could have prior context: past decisions, preferences, conventions, or anything the user says they told you before. Do this even when you could answer from the code, because the reasoning behind a decision is usually recorded only in memory. Searching and finding nothing is cheap; answering from the code and contradicting a past decision is not. Call \`add_memory\` when a decision, preference, convention, fact, or learning is worth recalling in a later session. Search with two or three distinctive keywords rather than a sentence, because every word must appear in a memory for it to match; if nothing comes back, search again with different words. Do not write memory files to disk: mem8 is the store."

exit 0
