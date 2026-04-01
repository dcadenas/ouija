#!/bin/bash
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0
RESP=$(curl -sf -X POST "http://localhost:${OUIJA_PORT:-7880}/api/hooks/session-start" \
  -H "Content-Type: application/json" \
  -d "{\"pane\":\"${PANE}\",\"cwd\":\"${PWD}\"}" 2>/dev/null) || exit 0
echo "$RESP" | jq -r '.output // empty' 2>/dev/null
# Inject pending prompt in background — polls for input prompt (❯) before
# injecting, so startup dialogs (large-file warnings) don't swallow the paste.
PROMPT=$(echo "$RESP" | jq -r '.pending_prompt // empty' 2>/dev/null)
if [ -n "$PROMPT" ]; then
  (
    # Wait up to 90s for the ❯ input prompt.
    # Skip first few seconds to avoid matching the shell ❯ before Claude loads.
    sleep 3
    for _ in $(seq 1 174); do
      if tmux capture-pane -t "$PANE" -p 2>/dev/null | grep -qF '❯'; then
        sleep 0.3
        printf '%s' "$PROMPT" | tmux load-buffer -b ouija-prompt -
        tmux paste-buffer -b ouija-prompt -t "$PANE" -d 2>/dev/null
        sleep 0.3
        tmux send-keys -t "$PANE" Enter 2>/dev/null
        exit 0
      fi
      sleep 0.5
    done
  ) &
fi
