#!/bin/bash
# Auto-unregister this Claude Code session from the ouija daemon.
# Runs as a SessionEnd hook when a session terminates.

PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
if [ -z "$PANE" ]; then
  echo "ok" >&2
  exit 0
fi

PORT="${OUIJA_PORT:-7880}"
BASE="http://localhost:${PORT}"

# Find session ID registered to this pane
SID=$(curl -sf "${BASE}/api/status" 2>/dev/null \
  | jq -r --arg pane "$PANE" '.sessions[] | select(.pane == $pane and .origin == "local") | .id' 2>/dev/null)

if [ -z "$SID" ]; then
  echo "ok" >&2
  exit 0
fi

curl -sf -X POST "${BASE}/api/remove" \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"${SID}\"}" >/dev/null 2>&1

# Clear tmux pane option so statusline doesn't show stale name
tmux set-option -pu -t "$PANE" @ouija_id 2>/dev/null

echo "ok" >&2
exit 0
