#!/bin/bash
# Auto-unregister this Claude Code session from the ouija daemon.
# Runs as a SessionEnd hook when a session terminates.
#
# Claude Code may cancel SessionEnd hooks quickly during /exit, so we
# background the actual work and exit immediately.

PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
if [ -z "$PANE" ]; then
  exit 0
fi

PORT="${OUIJA_PORT:-7880}"
BASE="http://localhost:${PORT}"

# Background the unregister so the hook returns immediately and isn't cancelled.
(
  SID=$(curl -sf "${BASE}/api/status" 2>/dev/null \
    | jq -r --arg pane "$PANE" '.sessions[] | select(.pane == $pane and .origin == "local") | .id' 2>/dev/null)

  [ -z "$SID" ] && exit 0

  curl -sf -X POST "${BASE}/api/remove" \
    -H "Content-Type: application/json" \
    -d "{\"id\":\"${SID}\"}" >/dev/null 2>&1

  tmux set-option -pu -t "$PANE" @ouija_id 2>/dev/null
) &
disown

exit 0
