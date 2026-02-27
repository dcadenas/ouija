#!/bin/bash
# UserPromptSubmit hook: notify daemon that user is active.
cat > /dev/null
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
PANE_NUM="${PANE#%}"
PORT="${OUIJA_PORT:-7880}"
curl -sf -X POST "http://127.0.0.1:${PORT}/api/pane/${PANE_NUM}/active" >/dev/null 2>&1
exit 0
