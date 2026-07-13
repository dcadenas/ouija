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
# Codex's shared app-server can inherit TMUX_PANE from the terminal that
# started it. Do not let that unrelated pane claim this SessionStart payload.
# The daemon repeats this check with project-root normalization; this raw path
# comparison is an early defense that avoids POSTing the obvious mismatch.
PANE_CWD=$(tmux display-message -p -t "$PANE" '#{pane_current_path}' 2>/dev/null)
[ -n "$PANE_CWD" ] && [ "$PANE_CWD" != "$CWD" ] && exit 0
BACKEND_SESSION_ID=$(printf '%s' "$PAYLOAD" | jq -r '.session_id // empty' 2>/dev/null)
[ -z "$BACKEND_SESSION_ID" ] && BACKEND_SESSION_ID="${CODEX_THREAD_ID:-}"
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "$(jq -cn --arg pane "$PANE" --arg cwd "$CWD" --arg backend_session_id "$BACKEND_SESSION_ID" --arg adapter "codex-cli" --arg launch_session_id "${OUIJA_SESSION_ID:-}" --arg launch_credential "${OUIJA_SESSION_START_CREDENTIAL:-}" \
    '{pane:$pane,cwd:$cwd,adapter:$adapter} + (if $launch_session_id == "" then {} else {launch_session_id:$launch_session_id} end) + (if $launch_credential == "" then {} else {launch_credential:$launch_credential} end) + (if $backend_session_id == "" then {} else {backend_session_id:$backend_session_id} end)')" 2>/dev/null) || exit 0
CONTEXT=$(printf '%s' "$RESP" | jq -r '.output // empty' 2>/dev/null)
[ -z "$CONTEXT" ] && exit 0
jq -cn --arg ctx "$CONTEXT" \
  '{hookSpecificOutput:{hookEventName:"SessionStart",additionalContext:$ctx}}'
