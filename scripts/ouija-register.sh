#!/bin/bash
# Auto-register this Claude Code session with the ouija daemon.
# Runs as a SessionStart hook on both startup and resume.
# Skips if the pane is already registered (avoids overwriting daemon pre-registration).

PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
[ -z "$PANE" ] && exit 0

# Read source from stdin JSON
SOURCE=$(jq -r '.source // "startup"' 2>/dev/null)

PORT="${OUIJA_PORT:-7880}"
BASE="http://localhost:${PORT}"

# Skip if ouija isn't running
STATUS=$(curl -sf "${BASE}/api/status" 2>/dev/null) || exit 0

# Check auto_register setting via API
AUTO=$(curl -sf "${BASE}/api/settings" 2>/dev/null | jq -r 'if .auto_register == false then "false" else "true" end')
[ "$AUTO" = "false" ] && exit 0

# Skip if this pane already has a registration (avoids racing with daemon pre-registration)
echo "$STATUS" | grep -q "\"pane\":\"${PANE}\"" && exit 0

# Auto-name from directory basename, sanitized
NAME=$(basename "$PWD" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9-' '-' | sed 's/^-//;s/-$//')
[ -z "$NAME" ] && NAME="unnamed"

# Set tmux pane option early so statusline picks it up before API responds
tmux set-option -p -t "$PANE" @ouija_id "$NAME" 2>/dev/null

ROLE="working on ${NAME}"

# Detect claude session ID from most recent .jsonl file
SLUG=$(echo "$PWD" | tr '/' '-')
SESSIONS_DIR="$HOME/.claude/projects/${SLUG}"
CLAUDE_SID=""
if [ -d "$SESSIONS_DIR" ]; then
  CLAUDE_SID=$(ls -t "$SESSIONS_DIR"/*.jsonl 2>/dev/null | head -1 | xargs -r basename 2>/dev/null | sed 's/\.jsonl$//')
fi

SID_FIELD=""
[ -n "$CLAUDE_SID" ] && SID_FIELD=",\"claude_session_id\":\"${CLAUDE_SID}\""

RESP=$(curl -sf -X POST "${BASE}/api/register" \
  -H "Content-Type: application/json" \
  -d "{\"id\":\"${NAME}\",\"pane\":\"${PANE}\",\"project_dir\":\"${PWD}\",\"role\":\"${ROLE}\"${SID_FIELD}}" 2>/dev/null)

# Parse the actual registered name (may differ from requested due to auto-suffix)
REGISTERED=$(echo "$RESP" | jq -r '.registered // empty' 2>/dev/null)
[ -z "$REGISTERED" ] && REGISTERED="$NAME"

# Store in tmux pane option so the statusline can read it instantly
tmux set-option -p -t "$PANE" @ouija_id "$REGISTERED" 2>/dev/null

echo "Registered as ${REGISTERED} on the ouija mesh."

# Version mismatch check: compare daemon version vs plugin cache version
DAEMON_VERSION=$(echo "$STATUS" | jq -r '.version // ""')
PLUGIN_VERSION=""
for d in "$HOME"/.claude/plugins/cache/ouija/ouija/*/; do
  [ -f "${d}.version" ] && PLUGIN_VERSION=$(cat "${d}.version" 2>/dev/null) && break
done
if [ -n "$DAEMON_VERSION" ] && [ -n "$PLUGIN_VERSION" ] && [ "$DAEMON_VERSION" != "$PLUGIN_VERSION" ]; then
  echo "WARNING: ouija version mismatch — daemon=${DAEMON_VERSION}, plugin=${PLUGIN_VERSION}."
  echo "  To fix: run 'ouija update' (or 'mise run use-local'), then start a new session."
  echo "  The current session's plugin cache is stale and won't update until restart."
fi

# Show mesh state so the session knows its peers
PEER_LINES=$(echo "$STATUS" | jq -r --arg self "$NAME" '
  [.sessions[]
   | select(.id != $self)
   | [.id] +
     (if .role and .role != "" then [.role] else [] end) +
     (if .bulletin and .bulletin != "" then ["bulletin: " + .bulletin] else [] end)
   | join(" | ")
  ] | sort[] // empty' 2>/dev/null)

if [ -n "$PEER_LINES" ]; then
  echo "Other sessions on the mesh:"
  echo "$PEER_LINES" | while IFS= read -r line; do
    echo "  - $line"
  done
else
  echo "No other sessions on the mesh."
fi
