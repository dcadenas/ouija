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

# Capture SID synchronously BEFORE backgrounding. On restart, the old session
# is already removed by restart_session's Remove event, so this returns empty
# and we skip the remove — preventing the race where the backgrounded subshell
# finds and removes the NEW session that was registered on the same pane.
SID=$(curl -sf "${BASE}/api/status" 2>/dev/null \
  | jq -r --arg pane "$PANE" '.sessions[] | select(.pane == $pane and .origin == "local") | .id' 2>/dev/null)

[ -z "$SID" ] && exit 0

# Background the actual remove so the hook returns immediately.
(
  curl -sf -X POST "${BASE}/api/remove" \
    -H "Content-Type: application/json" \
    -d "{\"id\":\"${SID}\"}" >/dev/null 2>&1

  tmux set-option -pu -t "$PANE" @ouija_id 2>/dev/null
) &
disown

exit 0
