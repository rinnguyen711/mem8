#!/usr/bin/env bash
# Re-state the memory instruction on each prompt.
#
# The SessionStart hook says the same thing once, at turn zero. In a long
# session that single line is buried under tool output long before it is needed,
# and the agent falls back to answering from the code — which is exactly the
# failure this plugin exists to prevent, because the reasoning behind a decision
# usually lives only in memory.
#
# Deliberately short: this text is added to every prompt, so it repeats the rule
# and nothing else. The full explanation stays in the session-start hook.
#
# Any failure must stay silent. A memory tool that breaks prompts is worse than
# one that is occasionally forgotten.

set -uo pipefail

command -v mem8 >/dev/null 2>&1 || exit 0

printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"%s"}}\n' \
  "mem8 holds memory from earlier sessions. If this turn touches a past decision, preference, or convention, call \`search_memory\` before answering, and \`add_memory\` if something worth recalling comes out of it."

exit 0
