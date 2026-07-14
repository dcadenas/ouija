#!/bin/bash
# Codex SessionStart hook: register this pane with the ouija daemon and surface
# mesh instructions back to the Codex TUI as SessionStart additionalContext.
# Codex passes the hook payload on stdin (session_id, cwd, ...) and TMUX_PANE in
# the environment. This hook does NOT unregister — cleanup relies on pane/process
# liveness.
PAYLOAD=$(cat)
LAUNCH_SESSION_ID="${OUIJA_SESSION_ID:-}"
LAUNCH_CREDENTIAL="${OUIJA_SESSION_START_CREDENTIAL:-}"
LAUNCH_CREDENTIAL_FILE=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --launch-session-id)
      LAUNCH_SESSION_ID="${2:-}"
      shift 2
      ;;
    --launch-credential-file)
      LAUNCH_CREDENTIAL_FILE="${2:-}"
      shift 2
      ;;
    *)
      exit 0
      ;;
  esac
done
# A session-flags command executes in Codex's shared app-server process, where
# command arguments and generated TOML may be observable. Claim the private
# proof file with an atomic rename, then read and remove only that claimed file.
# A concurrent duplicate hook cannot read the original path after the rename.
if [ -n "$LAUNCH_CREDENTIAL_FILE" ]; then
  CLAIMED_CREDENTIAL_FILE="${LAUNCH_CREDENTIAL_FILE}.$$.claimed"
  if mv -- "$LAUNCH_CREDENTIAL_FILE" "$CLAIMED_CREDENTIAL_FILE" 2>/dev/null; then
    trap 'rm -f -- "$CLAIMED_CREDENTIAL_FILE"' EXIT
    LAUNCH_CREDENTIAL=$(cat -- "$CLAIMED_CREDENTIAL_FILE" 2>/dev/null)
    rm -f -- "$CLAIMED_CREDENTIAL_FILE"
    trap - EXIT
  else
    LAUNCH_CREDENTIAL=""
  fi
fi
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
CWD=$(printf '%s' "$PAYLOAD" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
# Codex's shared app-server can inherit TMUX_PANE from the terminal that
# started it. Do not let that unrelated pane claim this SessionStart payload.
# The daemon repeats this check with project-root normalization; this raw path
# comparison is an early defense that avoids POSTing the obvious mismatch.
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$BACKEND_SESSION_ID" ] && BACKEND_SESSION_ID="${CODEX_THREAD_ID:-}"
PANE_CWD=$(tmux display-message -p -t "$PANE" '#{pane_current_path}' 2>/dev/null)
# A complete managed proof authorizes the named launch; inherited pane/CWD
# state is not allowed to suppress that claim. Legacy registration still keeps
# the pane check below.
if [ -z "$LAUNCH_SESSION_ID" ] || [ -z "$LAUNCH_CREDENTIAL" ] || [ -z "$BACKEND_SESSION_ID" ]; then
  [ -n "$PANE" ] && [ -n "$PANE_CWD" ] && [ "$PANE_CWD" != "$CWD" ] && exit 0
fi
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg pane "$PANE" --arg cwd "$CWD" --arg backend_session_id "$BACKEND_SESSION_ID" --arg adapter "codex-cli" --arg launch_session_id "$LAUNCH_SESSION_ID" --arg launch_credential "$LAUNCH_CREDENTIAL" \
    '{pane:$pane,cwd:$cwd,adapter:$adapter} + (if $launch_session_id == "" then {} else {launch_session_id:$launch_session_id} end) + (if $launch_credential == "" then {} else {launch_credential:$launch_credential} end) + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id,backend_identity:{backend:$adapter,session_id:$backend_session_id}} end)')" 2>/dev/null) || exit 0
CONTEXT=$(printf '%s' "$RESP" | jq -r '.output // empty' 2>/dev/null)
[ -z "$CONTEXT" ] && exit 0
jq -cn --arg ctx "$CONTEXT" \
  '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
