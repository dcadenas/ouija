#!/bin/bash
# Codex UserPromptSubmit hook: signal session activity to the ouija daemon (resets
# idle / watchdog timers) and, if the daemon returns context, surface it to the
# Codex TUI as UserPromptSubmit additionalContext. Payload arrives on stdin.
cat > /dev/null
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/prompt-submit" \
  -H "Content-Type: application/json" -d "{\"pane\":\"${PANE}\"}" 2>/dev/null)
CONTEXT=$(printf '%s' "$RESP" | jq -r '.output // empty' 2>/dev/null)
[ -z "$CONTEXT" ] && exit 0
jq -cn --arg ctx "$CONTEXT" \
  '{hookSpecificOutput:{hookEventName:"UserPromptSubmit",additionalContext:$ctx}}'
