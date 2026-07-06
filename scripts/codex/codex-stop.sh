#!/bin/bash
# Codex Stop hook: turn-scoped. Codex fires Stop after every assistant turn, so
# this must NOT unregister the session (unlike Claude's SessionEnd). It only pings
# the daemon's turn-stop endpoint (pending-reply / idle bookkeeping) and returns
# {"continue":true} so Codex proceeds normally. Payload arrives on stdin.
cat > /dev/null
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
if [ -n "$PANE" ]; then
  curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/stop" \
    -H "Content-Type: application/json" -d "{\"pane\":\"${PANE}\"}" >/dev/null 2>&1 || true
fi
printf '%s\n' '{"continue":true}'
