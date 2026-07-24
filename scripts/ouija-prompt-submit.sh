#!/bin/bash
cat > /dev/null
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
BODY=$(jq -cn --arg pane "$PANE" --arg incarnation "${OUIJA_SESSION_INCARNATION:-}" \
  '{pane:$pane} + (if $incarnation == "" then {} else {session_incarnation:$incarnation} end)')
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/prompt-submit" \
  -H "Content-Type: application/json" -d "$BODY" 2>/dev/null)
[ -n "$RESP" ] && echo "$RESP" | jq -r '.output // empty' 2>/dev/null
echo "ok" >&2
