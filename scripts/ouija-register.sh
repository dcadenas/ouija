#!/bin/bash
PAYLOAD=$(cat)
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
CWD=$(printf '%s' "$PAYLOAD" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg pane "$PANE" --arg cwd "$CWD" --arg backend_session_id "$BACKEND_SESSION_ID" --arg adapter "claude-code" --arg launch_session_id "${OUIJA_SESSION_ID:-}" \
    '{pane:$pane,cwd:$cwd,adapter:$adapter} + (if $launch_session_id == "" then {} else {launch_session_id:$launch_session_id} end) + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id} end)')" 2>/dev/null) || exit 0
echo "$RESP" | jq -r '.output // empty' 2>/dev/null
