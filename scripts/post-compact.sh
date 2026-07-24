#!/bin/bash
PAYLOAD=$(cat 2>/dev/null || echo '{}')
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
INCARNATION="${OUIJA_SESSION_INCARNATION:-}"
if [ -n "$BACKEND_SESSION_ID" ]; then
  SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  STORED_INCARNATION=$("$SCRIPT_DIR/hook-incarnation.sh" load "$BACKEND_SESSION_ID")
  [ -n "$STORED_INCARNATION" ] && INCARNATION="$STORED_INCARNATION"
fi
BODY=$(jq -cn --arg pane "$PANE" --arg backend_session_id "$BACKEND_SESSION_ID" --arg incarnation "$INCARNATION" \
  '{pane:$pane} + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id} end) + (if $incarnation == "" then {} else {session_incarnation:$incarnation} end)')
curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/post-compact" \
  -H "Content-Type: application/json" -d "$BODY" >/dev/null 2>&1 || true
