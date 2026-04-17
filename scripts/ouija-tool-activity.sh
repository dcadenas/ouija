#!/bin/bash
# PreToolUse hook: signal activity to the daemon so the idle timer resets.
# Fire-and-forget — never blocks tool execution.
INPUT=$(cat 2>/dev/null || echo '{}')
TOOL=$(echo "$INPUT" | jq -r '.tool_name // "unknown"' 2>/dev/null)
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/pre-tool-use" \
  -H "Content-Type: application/json" \
  -d "{\"pane\":\"${PANE}\",\"tool_name\":\"${TOOL}\"}" >/dev/null 2>&1 || true
exit 0
