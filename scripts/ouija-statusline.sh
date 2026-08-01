#!/bin/bash
# Ouija mesh status line for Claude Code.
# Receives JSON session data on stdin from Claude Code.

# Read stdin JSON (Claude Code sends session data)
INPUT=$(cat)

PORT="${OUIJA_PORT:-7880}"
STATUS=$(curl -sf "http://localhost:${PORT}/api/status" 2>/dev/null) || { echo "ouija | offline"; exit 0; }

PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"

CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
[ -z "$CWD" ] && CWD="$PWD"
PROJECT_DIR=$(git -C "$CWD" rev-parse --path-format=absolute --show-toplevel 2>/dev/null || printf '%s' "$CWD")

# The daemon row is authoritative. Pane markers survive process replacement,
# and basename guesses cannot account for daemon-assigned collision suffixes.
MY_ID=""
if [ -n "$PANE" ]; then
  DAEMON_ROW=$(printf '%s' "$STATUS" | jq -c \
    --arg pane "$PANE" --arg project "$PROJECT_DIR" \
    '.sessions[] | select(.pane == $pane and .project_dir == $project)' 2>/dev/null)
  if [ -n "$DAEMON_ROW" ]; then
    MY_ID=$(printf '%s' "$DAEMON_ROW" | jq -r '.id // empty' 2>/dev/null)
  fi
fi

# Peer counts (local + remote, excluding self)
if [ -n "$MY_ID" ]; then
  LOCAL_PEERS=$(echo "$STATUS" | jq --arg me "$MY_ID" '[.sessions[] | select(.id != $me and .origin == "local")] | length' 2>/dev/null)
  REMOTE_PEERS=$(echo "$STATUS" | jq '[.sessions[] | select(.origin != "local" and .origin != "human")] | length' 2>/dev/null)
else
  LOCAL_PEERS=$(echo "$STATUS" | jq '[.sessions[] | select(.origin == "local")] | length' 2>/dev/null)
  REMOTE_PEERS=$(echo "$STATUS" | jq '[.sessions[] | select(.origin != "local" and .origin != "human")] | length' 2>/dev/null)
fi

# Version
DAEMON_V=$(echo "$STATUS" | jq -r '.version // ""' 2>/dev/null)
PLUGIN_V=""
for d in "$HOME"/.claude/plugins/cache/ouija/ouija/*/; do
  [ -f "${d}.version" ] && PLUGIN_V=$(cat "${d}.version" 2>/dev/null) && break
done

# Build parts
PARTS=()

if [ -n "$MY_ID" ]; then
  PARTS+=("ouija id: $MY_ID")
elif [ -n "$PANE" ]; then
  PARTS+=("ouija id: \033[33mregistering…\033[0m")
else
  PARTS+=("ouija id: \033[33munregistered\033[0m")
fi

if [ "${REMOTE_PEERS:-0}" -gt 0 ]; then
  PARTS+=("peers: ${LOCAL_PEERS:-0} local + ${REMOTE_PEERS} remote")
else
  PARTS+=("peers: ${LOCAL_PEERS:-0}")
fi

if [ -n "$DAEMON_V" ] && [ -n "$PLUGIN_V" ] && [ "$DAEMON_V" != "$PLUGIN_V" ]; then
  PARTS+=("\033[33m⚠ daemon=${DAEMON_V} plugin=${PLUGIN_V}\033[0m")
else
  PARTS+=("v${DAEMON_V}")
fi

echo -e "$(IFS='|'; echo "${PARTS[*]}" | sed 's/|/ | /g')"
