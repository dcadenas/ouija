#!/bin/bash
PAYLOAD=$(cat)
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
CWD=$(printf '%s' "$PAYLOAD" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg pane "$PANE" --arg cwd "$CWD" --arg backend_session_id "$BACKEND_SESSION_ID" --arg adapter "claude-code" --arg launch_session_id "${OUIJA_SESSION_ID:-}" --arg launch_credential "${OUIJA_SESSION_START_CREDENTIAL:-}" --arg incarnation "${OUIJA_SESSION_INCARNATION:-}" \
    '{pane:$pane,cwd:$cwd,adapter:$adapter} + (if $launch_session_id == "" then {} else {launch_session_id:$launch_session_id} end) + (if $launch_credential == "" then {} else {launch_credential:$launch_credential} end) + (if $incarnation == "" then {} else {session_incarnation:$incarnation} end) + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id,backend_identity:{backend:$adapter,session_id:$backend_session_id}} end)')" 2>/dev/null) || exit 0
SESSION_INCARNATION=$(printf '%s' "$RESP" | jq -r '.session_incarnation // empty' 2>/dev/null)
if [ -n "$BACKEND_SESSION_ID" ] && [ -n "$SESSION_INCARNATION" ]; then
  SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  "$SCRIPT_DIR/hook-incarnation.sh" store "$BACKEND_SESSION_ID" "$SESSION_INCARNATION"
fi
echo "$RESP" | jq -r '.output // empty' 2>/dev/null
