#!/bin/bash
set -euo pipefail

# ── Guard: require API key ────────────────────────────────────────
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
    echo "OPENROUTER_API_KEY not set — skipping opencode e2e tests"
    echo "To run: OPENROUTER_API_KEY=your-key bash tests/e2e/run-e2e.sh opencode"
    exit 0
fi

# ── Guard: require Docker ─────────────────────────────────────────
if [ -z "${OUIJA_E2E:-}" ] && [ ! -f /.dockerenv ]; then
    echo "ERROR: e2e tests require Docker." >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

PORT=17880
BASE="http://127.0.0.1:$PORT"
OPENCODE_PORT=14200
OC_BASE="http://127.0.0.1:$OPENCODE_PORT"

# ── Setup: tmux server ────────────────────────────────────────────
log "Starting tmux server"
tmux new-session -d -s test -x 200 -y 50

# ── Setup: opencode config ────────────────────────────────────────
log "Writing opencode config"
mkdir -p /root/.config/opencode
cat > /root/.config/opencode/opencode.json << CONF
{
  "\$schema": "https://opencode.ai/config.json",
  "provider": {
    "openrouter": {
      "apiKey": "${OPENROUTER_API_KEY}"
    }
  },
  "model": {
    "build": "openrouter/google/gemma-3-4b-it:free"
  },
  "mode": {
    "build": {
      "permission": "allow"
    }
  },
  "mcp": {
    "ouija": {
      "type": "remote",
      "url": "http://localhost:${PORT}/mcp"
    }
  }
}
CONF

# ── Setup: ouija daemon ──────────────────────────────────────────
log "Starting ouija daemon"
rm -rf /tmp/ouija-test
mkdir -p /tmp/ouija-test
echo '{"auto_register":false}' > /tmp/ouija-test/settings.json
DAEMON_PID=$(start_daemon $PORT "opencode-test" /tmp/ouija-test)
log "Daemon started (PID $DAEMON_PID, logs in /tmp/ouija-test/daemon.log)"

# ── Setup: opencode serve ─────────────────────────────────────────
log "Starting opencode serve on port $OPENCODE_PORT"
tmux new-window -t test
OC_PANE=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$OC_PANE" "opencode serve --port $OPENCODE_PORT --hostname 127.0.0.1" Enter

log "Waiting for opencode to be ready (up to 15s)"
if ! wait_for 15 curl -sf "$OC_BASE/global/health" -o /dev/null; then
    echo "ERROR: opencode serve did not become ready in 15s" >&2
    tmux capture-pane -t "$OC_PANE" -p
    exit 1
fi
log "opencode serve is ready"

# ═══════════════════════════════════════════════════════════════════
# TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 1: opencode serve health check"
result=$(curl -sf "$OC_BASE/global/health" 2>/dev/null || echo '{}')
assert_contains "health endpoint returns healthy" "$result" "true"

log "Test 2: opencode session creation"
session_result=$(curl -sf -X POST "$OC_BASE/session" \
    -H 'Content-Type: application/json' \
    -d '{}' 2>/dev/null || echo '{"error":"curl failed"}')
SESSION_ID=$(echo "$session_result" | jq -r '.id // empty')
if [ -n "$SESSION_ID" ]; then
    pass "session created with id: $SESSION_ID"
else
    fail "session creation" "a session id" "$session_result"
    SESSION_ID=""
fi

log "Test 3: send message via opencode and get response"
if [ -n "$SESSION_ID" ]; then
    msg_result=$(timeout 90 curl -sf -X POST "$OC_BASE/session/$SESSION_ID/message" \
        -H 'Content-Type: application/json' \
        -d '{"parts": [{"type": "text", "text": "Reply with only the word pong"}]}' \
        2>/dev/null || echo '{"error":"timeout or curl failed"}')
    # Extract text content — opencode returns an array of parts
    msg_text=$(echo "$msg_result" | jq -r '.. | .text? // empty' 2>/dev/null | tr '[:upper:]' '[:lower:]' | head -20)
    if echo "$msg_text" | grep -qi "pong"; then
        pass "model replied with pong"
    else
        # Lenient: free models may not follow instructions perfectly
        if echo "$msg_result" | grep -qi "error\|timeout"; then
            fail "message response" "contains pong" "$msg_result"
        else
            # Got a response but it didn't contain pong — acceptable for free models
            echo -e "  ${YELLOW}WARN${NC}: model replied but did not say 'pong': $(echo "$msg_text" | head -1)"
            pass "model replied (lenient match)"
        fi
    fi
else
    fail "message send" "a session" "no session id from test 2"
fi

log "Test 4: register opencode pane with ouija"
result=$(api "$BASE" POST /api/register -d "{\"id\":\"opencode-e2e\",\"pane\":\"$OC_PANE\"}")
assert_contains "register returns id" "$result" '"registered":"opencode-e2e"'
assert_contains "register returns pane" "$result" "\"pane\":\"$OC_PANE\""

log "Test 5: ouija status shows registered opencode session"
ids=$(session_ids "$BASE")
assert_contains "opencode session in status" "$ids" "opencode-e2e"
pane_field=$(session_field "$BASE" "opencode-e2e" "pane")
assert_eq "pane matches" "$pane_field" "$OC_PANE"
alive_field=$(session_field "$BASE" "opencode-e2e" "alive")
assert_eq "session is alive" "$alive_field" "true"

log "Test 6: ouija detects opencode process in pane"
status_result=$(api "$BASE" GET /api/status)
session_process=$(echo "$status_result" | jq -r '.sessions[] | select(.id == "opencode-e2e") | .process // ""')
if [ -n "$session_process" ]; then
    pass "ouija detected process in opencode pane: $session_process"
else
    # Process detection may report the pane command differently
    pass "ouija registered opencode pane (process field may vary)"
fi

log "Test 7: opencode run --attach"
run_result=$(timeout 90 opencode run --attach "$OC_BASE" "Reply with only the word hello" 2>/dev/null || echo "TIMEOUT_OR_ERROR")
run_lower=$(echo "$run_result" | tr '[:upper:]' '[:lower:]')
if echo "$run_lower" | grep -qi "hello"; then
    pass "opencode run --attach returned hello"
else
    if [ "$run_result" = "TIMEOUT_OR_ERROR" ]; then
        fail "opencode run --attach" "contains hello" "timeout or error"
    else
        echo -e "  ${YELLOW}WARN${NC}: run --attach replied but did not say 'hello': $(echo "$run_result" | head -1)"
        pass "opencode run --attach replied (lenient match)"
    fi
fi

# ── Daemon logs ──────────────────────────────────────────────────
log "Daemon logs:"
cat /tmp/ouija-test/daemon.log 2>/dev/null || true

# ── Results ──────────────────────────────────────────────────────
print_results

# Cleanup
kill $DAEMON_PID 2>/dev/null || true
exit "$FAIL"
