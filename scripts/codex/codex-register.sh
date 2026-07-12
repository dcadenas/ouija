#!/bin/bash
# Codex SessionStart hook: register this pane with the ouija daemon and surface
# mesh instructions back to the Codex TUI as SessionStart additionalContext.
# Codex passes the hook payload on stdin (session_id, cwd, ...) and TMUX_PANE in
# the environment. This hook does NOT unregister — cleanup relies on pane/process
# liveness.
PAYLOAD=$(cat)
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
CWD=$(printf '%s' "$PAYLOAD" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$BACKEND_SESSION_ID" ] && BACKEND_SESSION_ID="${CODEX_THREAD_ID:-}"
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg pane "$PANE" --arg cwd "$CWD" --arg backend_session_id "$BACKEND_SESSION_ID" \
    '{pane:$pane,cwd:$cwd} + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id} end)')" 2>/dev/null) || exit 0
CONTEXT=$(printf '%s' "$RESP" | jq -r '.output // empty' 2>/dev/null)
[ -z "$CONTEXT" ] && exit 0
jq -cn --arg ctx "$CONTEXT" \
  '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
