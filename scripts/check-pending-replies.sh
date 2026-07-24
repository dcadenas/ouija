#!/bin/bash
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
BODY=$(jq -cn --arg pane "$PANE" --arg incarnation "${OUIJA_SESSION_INCARNATION:-}" \
  '{pane:$pane} + (if $incarnation == "" then {} else {session_incarnation:$incarnation} end)')
curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/stop" \
  -H "Content-Type: application/json" -d "$BODY" >/dev/null 2>&1 || true
