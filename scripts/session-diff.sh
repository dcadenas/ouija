#!/bin/bash
# Show mesh changes since last check: joins, leaves, metadata updates, stale self-check.

PORT="${OUIJA_PORT:-7880}"
PANE="${TMUX_PANE:-$(tmux display-message -p '#{pane_id}' 2>/dev/null)}"
if [ -z "$PANE" ]; then
  echo "ok" >&2
  exit 0
fi

CACHE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/ouija"
mkdir -p "$CACHE_DIR"
CACHE_FILE="$CACHE_DIR/sessions-${PANE}.json"

STATUS=$(curl -sf "http://localhost:${PORT}/api/status" 2>/dev/null)
if [ -z "$STATUS" ]; then
  echo "ok" >&2
  exit 0
fi

CURRENT=$(echo "$STATUS" | jq -c '[.sessions[] | {id, origin: (.origin | split("(") | .[0]), role, bulletin}] | sort_by(.id)' 2>/dev/null)
if [ -z "$CURRENT" ]; then
  echo "ok" >&2
  exit 0
fi

OUTPUT=""

# --- Stale metadata self-check ---
MY_SESSION=$(echo "$STATUS" | jq -c --arg pane "$PANE" '.sessions[] | select(.pane == $pane)' 2>/dev/null)
if [ -n "$MY_SESSION" ]; then
  IS_STALE=$(echo "$MY_SESSION" | jq -r '.stale')
  if [ "$IS_STALE" = "true" ]; then
    MY_ID=$(echo "$MY_SESSION" | jq -r '.id')
    MY_ROLE=$(echo "$MY_SESSION" | jq -r '.role // "none"')
    MY_BULLETIN=$(echo "$MY_SESSION" | jq -r '.bulletin // ""')
    if [ -n "$MY_BULLETIN" ]; then
      OUTPUT="[ouija] Your metadata is stale. Current: role=\"${MY_ROLE}\" | bulletin=\"${MY_BULLETIN}\". Call session_update(id=\"${MY_ID}\", role=\"<what you're doing now>\", bulletin=\"<what you can help with or need>\") if these are outdated."
    else
      OUTPUT="[ouija] Your metadata is stale (role: \"${MY_ROLE}\", no bulletin). Call session_update(id=\"${MY_ID}\", role=\"<what you're doing now>\", bulletin=\"<what you can help with or need>\") to stay discoverable."
    fi
  fi
fi

# --- Session diff ---
if [ ! -f "$CACHE_FILE" ]; then
  echo "$CURRENT" > "$CACHE_FILE"
  [ -n "$OUTPUT" ] && echo "$OUTPUT"
  echo "ok" >&2
  exit 0
fi

PREVIOUS=$(cat "$CACHE_FILE")
echo "$CURRENT" > "$CACHE_FILE"

if [ "$CURRENT" != "$PREVIOUS" ]; then
  CURRENT_IDS=$(echo "$CURRENT" | jq -r '.[].id' | sort)
  PREVIOUS_IDS=$(echo "$PREVIOUS" | jq -r '.[].id' | sort)

  # Helper to format a session line
  fmt_session() {
    echo "$1" | jq -r --arg id "$2" '.[] | select(.id == $id) | "  - \(.id) (\(.origin))\(if .role then " — " + .role else "" end)\(if .bulletin then " | bulletin: " + .bulletin else "" end)"'
  }

  # Joined sessions
  JOINED_IDS=$(comm -13 <(echo "$PREVIOUS_IDS") <(echo "$CURRENT_IDS"))
  if [ -n "$JOINED_IDS" ]; then
    [ -n "$OUTPUT" ] && OUTPUT="$OUTPUT
"
    OUTPUT="${OUTPUT}[ouija mesh] joined:"
    while IFS= read -r sid; do
      OUTPUT="$OUTPUT
$(fmt_session "$CURRENT" "$sid")"
    done <<< "$JOINED_IDS"
  fi

  # Left sessions
  LEFT_IDS=$(comm -23 <(echo "$PREVIOUS_IDS") <(echo "$CURRENT_IDS"))
  if [ -n "$LEFT_IDS" ]; then
    [ -n "$OUTPUT" ] && OUTPUT="$OUTPUT
"
    OUTPUT="${OUTPUT}[ouija mesh] left: $(echo "$LEFT_IDS" | tr '\n' ', ' | sed 's/,$//')"
  fi

  # Metadata changes on existing sessions
  COMMON_IDS=$(comm -12 <(echo "$PREVIOUS_IDS") <(echo "$CURRENT_IDS"))
  CHANGES=""
  while IFS= read -r sid; do
    [ -z "$sid" ] && continue
    OLD=$(echo "$PREVIOUS" | jq -c --arg id "$sid" '.[] | select(.id == $id) | {role, bulletin}')
    NEW=$(echo "$CURRENT" | jq -c --arg id "$sid" '.[] | select(.id == $id) | {role, bulletin}')
    [ "$OLD" = "$NEW" ] && continue

    OLD_ROLE=$(echo "$OLD" | jq -r '.role // ""')
    NEW_ROLE=$(echo "$NEW" | jq -r '.role // ""')
    OLD_BULLETIN=$(echo "$OLD" | jq -r '.bulletin // ""')
    NEW_BULLETIN=$(echo "$NEW" | jq -r '.bulletin // ""')

    DETAIL=""
    [ "$OLD_ROLE" != "$NEW_ROLE" ] && DETAIL="role: ${NEW_ROLE:-<cleared>}"
    if [ "$OLD_BULLETIN" != "$NEW_BULLETIN" ]; then
      [ -n "$DETAIL" ] && DETAIL="$DETAIL, "
      DETAIL="${DETAIL}bulletin: ${NEW_BULLETIN:-<cleared>}"
    fi
    CHANGES="$CHANGES
  - $sid: $DETAIL"
  done <<< "$COMMON_IDS"

  if [ -n "$CHANGES" ]; then
    [ -n "$OUTPUT" ] && OUTPUT="$OUTPUT
"
    OUTPUT="${OUTPUT}[ouija mesh] updated:$CHANGES"
  fi
fi

[ -n "$OUTPUT" ] && echo "$OUTPUT"
echo "ok" >&2
exit 0
