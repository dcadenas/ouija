#!/bin/bash
INPUT=$(cat)
TOOL=$(echo "$INPUT" | jq -r '.tool_name // "unknown"' 2>/dev/null)
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && { echo "ok" >&2; exit 0; }
BODY=$(jq -cn --arg pane "$PANE" --arg tool_name "$TOOL" --arg incarnation "${OUIJA_SESSION_INCARNATION:-}" \
  '{pane:$pane,tool_name:$tool_name} + (if $incarnation == "" then {} else {session_incarnation:$incarnation} end)')
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/pre-tool-use" \
  -H "Content-Type: application/json" \
  -d "$BODY" 2>/dev/null)
BLOCK=$(echo "$RESP" | jq -r '.block // false' 2>/dev/null)
[ "$BLOCK" != "true" ] && { echo "ok" >&2; exit 0; }
echo "$RESP" | jq -r '.message' 2>/dev/null >&2
exit 2
