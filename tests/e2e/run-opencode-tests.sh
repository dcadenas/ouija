#!/bin/bash
set -euo pipefail

# ── Guard: require Docker ─────────────────────────────────────────
if [ -z "${OUIJA_E2E:-}" ] && [ ! -f /.dockerenv ]; then
    echo "ERROR: e2e tests require Docker for tmux isolation." >&2
    echo "Run:  bash tests/e2e/run-e2e.sh opencode" >&2
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
  "model": "opencode/gpt-5-nano",
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
# TESTS — fast tests first, then slow LLM round-trips
# ═══════════════════════════════════════════════════════════════════

log "Test 1: opencode session creation"
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

log "Test 2: ouija daemon is alive alongside opencode"
ouija_status=$(curl -sf "$BASE/api/status" 2>/dev/null || echo '{"error":"curl failed"}')
assert_contains "ouija daemon responds" "$ouija_status" '"daemon"'

log "Test 3: ouija MCP server accessible"
mcp_health=$(mcp_init "$BASE")
if echo "$mcp_health" | grep -q "ouija"; then
    pass "ouija MCP server responds to initialize"
else
    fail "ouija MCP reachable" "contains ouija" "$mcp_health"
fi

log "Test 4: send message via opencode API and get response"
if [ -n "$SESSION_ID" ]; then
    msg_result=$(timeout 90 curl -sf -X POST "$OC_BASE/session/$SESSION_ID/message" \
        -H 'Content-Type: application/json' \
        -d '{"parts": [{"type": "text", "text": "Reply with only the word pong"}]}' \
        2>/dev/null || echo '{"error":"timeout or curl failed"}')
    msg_text=$(echo "$msg_result" | jq -r '.. | .text? // empty' 2>/dev/null | tr '[:upper:]' '[:lower:]' | head -20)
    if echo "$msg_text" | grep -qi "pong"; then
        pass "model replied with pong"
    else
        if echo "$msg_result" | grep -qi "error\|timeout"; then
            fail "message response" "contains pong" "$(echo "$msg_result" | head -c 200)"
        else
            echo -e "  ${YELLOW}WARN${NC}: model replied but did not say 'pong': $(echo "$msg_text" | head -1)"
            pass "model replied (lenient match)"
        fi
    fi
else
    fail "message send" "a session" "no session id from test 1"
fi

log "Test 5: send second message to same session"
if [ -n "$SESSION_ID" ]; then
    msg2_result=$(timeout 90 curl -sf -X POST "$OC_BASE/session/$SESSION_ID/message" \
        -H 'Content-Type: application/json' \
        -d '{"parts": [{"type": "text", "text": "What is 2+2? Reply with just the number."}]}' \
        2>/dev/null || echo '{"error":"timeout or curl failed"}')
    msg2_text=$(echo "$msg2_result" | jq -r '.. | .text? // empty' 2>/dev/null | head -5)
    if echo "$msg2_text" | grep -q "4"; then
        pass "model answered 2+2=4"
    else
        if echo "$msg2_result" | grep -qi "error\|timeout"; then
            fail "second message" "contains 4" "$(echo "$msg2_result" | head -c 200)"
        else
            pass "model replied to second message (lenient)"
        fi
    fi
else
    fail "second message" "a session" "no session id"
fi

log "Test 6: opencode session list shows conversations"
sessions=$(curl -sf "$OC_BASE/session" 2>/dev/null || echo '[]')
session_count=$(echo "$sessions" | jq 'length' 2>/dev/null || echo 0)
if [ "$session_count" -ge 1 ]; then
    pass "opencode has $session_count session(s)"
else
    fail "session count" ">=1" "$session_count"
fi

# ── Daemon logs ──────────────────────────────────────────────────
log "Daemon logs:"
cat /tmp/ouija-test/daemon.log 2>/dev/null || true

# ── Results ──────────────────────────────────────────────────────
print_results

# Cleanup
kill $DAEMON_PID 2>/dev/null || true
exit "$FAIL"
