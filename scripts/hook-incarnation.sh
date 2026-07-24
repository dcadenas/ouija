#!/bin/bash
# Persist the daemon-issued lifecycle incarnation for one backend thread.
# Records are private to this user and keyed by a digest of the native thread
# ID, never by a reusable public Ouija ID or tmux pane.

ACTION="${1:-}"
BACKEND_SESSION_ID="${2:-}"
[ -z "$ACTION" ] && exit 0
[ -z "$BACKEND_SESSION_ID" ] && exit 0

if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  STORE_DIR="$XDG_RUNTIME_DIR/ouija-hook-incarnations"
elif [ -n "${HOME:-}" ]; then
  STORE_DIR="$HOME/.ouija/run/hook-incarnations"
else
  exit 0
fi

if command -v sha256sum >/dev/null 2>&1; then
  KEY=$(printf '%s' "$BACKEND_SESSION_ID" | sha256sum | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  KEY=$(printf '%s' "$BACKEND_SESSION_ID" | shasum -a 256 | awk '{print $1}')
else
  exit 0
fi
[ -z "$KEY" ] && exit 0

umask 077
mkdir -p "$STORE_DIR" 2>/dev/null || exit 0
chmod 700 "$STORE_DIR" 2>/dev/null || exit 0
TOKEN_FILE="$STORE_DIR/$KEY.json"

case "$ACTION" in
  store)
    INCARNATION="${3:-}"
    case "$INCARNATION" in
      ''|*[!0-9]*) exit 0 ;;
    esac
    TEMP_FILE=$(mktemp "$STORE_DIR/.incarnation.XXXXXX") || exit 0
    trap 'rm -f -- "$TEMP_FILE"' EXIT
    jq -cn --arg backend_session_id "$BACKEND_SESSION_ID" \
      --arg incarnation "$INCARNATION" \
      '{backend_session_id:$backend_session_id,incarnation:$incarnation}' \
      > "$TEMP_FILE" || exit 0
    chmod 600 "$TEMP_FILE" 2>/dev/null || exit 0
    mv -f -- "$TEMP_FILE" "$TOKEN_FILE" 2>/dev/null || exit 0
    trap - EXIT
    ;;
  load)
    [ -f "$TOKEN_FILE" ] || exit 0
    jq -r --arg backend_session_id "$BACKEND_SESSION_ID" \
      'select(.backend_session_id == $backend_session_id) | .incarnation // empty' \
      "$TOKEN_FILE" 2>/dev/null
    ;;
  delete)
    EXPECTED_INCARNATION="${3:-}"
    [ -f "$TOKEN_FILE" ] || exit 0
    STORED_INCARNATION=$(jq -r --arg backend_session_id "$BACKEND_SESSION_ID" \
      'select(.backend_session_id == $backend_session_id) | .incarnation // empty' \
      "$TOKEN_FILE" 2>/dev/null)
    if [ -n "$EXPECTED_INCARNATION" ] && [ "$STORED_INCARNATION" = "$EXPECTED_INCARNATION" ]; then
      rm -f -- "$TOKEN_FILE"
    fi
    ;;
esac
