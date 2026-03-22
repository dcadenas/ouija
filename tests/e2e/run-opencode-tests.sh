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

# ═══════════════════════════════════════════════════════════════════
# OUIJA INTEGRATION TESTS — session_start + session_send via HTTP API
# Stop the standalone serve first so the shared serve can start cleanly.
# ═══════════════════════════════════════════════════════════════════
log "Stopping standalone opencode serve for integration tests"
pkill -f "opencode serve --port $OPENCODE_PORT" 2>/dev/null || true
sleep 2

log "Test 7: ouija session_start with backend=opencode"
start_result=$(mcp_call_tool "$BASE" "session_start" \
    '{"name":"oc-e2e","project_dir":"/tmp","backend":"opencode"}')
if echo "$start_result" | grep -q "started.*oc-e2e"; then
    pass "ouija started opencode session 'oc-e2e'"
else
    fail "session_start opencode" "contains 'started'" "$(echo "$start_result" | head -c 200)"
fi

log "Test 8: ouija detects opencode serve readiness"
# Wait for the session to be fully registered with serve_port
sleep 5
oc_status=$(api "$BASE" GET /api/status)
oc_session=$(echo "$oc_status" | jq -r '.sessions[] | select(.id == "oc-e2e")')
if [ -n "$oc_session" ]; then
    pass "oc-e2e session registered in ouija"
else
    fail "oc-e2e registration" "session exists" "not found in status"
fi

log "Test 8b: backend-session readiness endpoint resolves registered session"
backend_sid=$(echo "$oc_status" | jq -r '.sessions[] | select(.id == "oc-e2e") | .backend_session_id // empty')
if [ -n "$backend_sid" ]; then
    bs_resolve=$(curl -sf -X POST "$BASE/api/backend-session/${backend_sid}/ready" \
        -H "Content-Type: application/json" -d '{}' 2>/dev/null || echo '{"error":"failed"}')
    if echo "$bs_resolve" | jq -r '.session // empty' 2>/dev/null | grep -q "oc-e2e"; then
        pass "backend-session endpoint resolved oc-e2e by backend_session_id"
    else
        fail "backend-session resolve" "session=oc-e2e" "$(echo "$bs_resolve" | head -c 200)"
    fi
else
    fail "backend_session_id" "non-empty value" "empty in status"
fi

log "Test 9: ouija session_send delivers to opencode via HTTP API"
send_result=$(mcp_call_tool "$BASE" "session_send" \
    '{"from":"test-sender","to":"oc-e2e","message":"Reply with only the word hello","expects_reply":false}')
if echo "$send_result" | grep -qi "delivered\|success"; then
    pass "ouija delivered message to opencode session"
else
    # Check daemon log for HTTP delivery confirmation
    sleep 5
    if grep -q "delivered message via prompt_async.*oc-e2e" /tmp/ouija-test/daemon.log 2>/dev/null; then
        pass "ouija delivered message via HTTP API (confirmed in daemon log)"
    else
        fail "session_send to opencode" "delivery via HTTP" "$(echo "$send_result" | head -c 200)"
    fi
fi

log "Test 10: opencode received and processed the message"
# Wait for the LLM to respond
sleep 15
# The shared serve port is daemon_port + 320
OC_SERVE_PORT=$((PORT + 320))
# Verify it's reachable
if curl -sf "http://127.0.0.1:${OC_SERVE_PORT}/global/health" -o /dev/null 2>/dev/null; then
    # Find the most recent session (the one created by session_start for oc-e2e)
    latest_session=$(curl -sf "http://127.0.0.1:${OC_SERVE_PORT}/session" 2>/dev/null \
        | jq -r 'sort_by(.time.updated) | last | .id // empty' 2>/dev/null)
    if [ -n "$latest_session" ]; then
        msgs=$(curl -sf "http://127.0.0.1:${OC_SERVE_PORT}/session/${latest_session}/message" 2>/dev/null || echo '[]')
        response_text=$(echo "$msgs" | jq -r '[.[] | select(.info.role == "assistant") | .parts[]? | select(.type == "text") | .text] | join(" ")' 2>/dev/null | tr '[:upper:]' '[:lower:]')
        if echo "$response_text" | grep -qi "hello"; then
            pass "opencode LLM replied with 'hello'"
        elif [ -n "$response_text" ]; then
            echo -e "  ${YELLOW}WARN${NC}: LLM replied but did not say 'hello': $(echo "$response_text" | head -c 100)"
            pass "opencode LLM replied (lenient match)"
        else
            fail "opencode response" "contains hello" "no response text found (session: $latest_session)"
        fi
    else
        fail "opencode sessions" "at least one session" "none found on port $OC_SERVE_PORT"
    fi
else
    fail "serve health" "serve reachable on port $OC_SERVE_PORT" "health check failed"
fi

log "Test 10b: prompt_async delivery confirmed without errors"
if grep -q "delivered message via prompt_async" /tmp/ouija-test/daemon.log 2>/dev/null; then
    pass "prompt_async delivery confirmed in daemon log"
else
    fail "prompt_async" "delivery log entry" "not found in daemon log"
fi
# Also check opencode serve log for Zod errors if available
OC_SERVE_LOG="$HOME/.local/share/ouija/opencode-serve.log"
if [ -f "$OC_SERVE_LOG" ]; then
    zod_errors=$(grep -c "invalid_union\|invalid_type.*received undefined" "$OC_SERVE_LOG" 2>/dev/null || echo "0")
    if [ "$zod_errors" -eq 0 ]; then
        pass "no Zod validation errors in opencode serve log"
    else
        fail "Zod errors" "0 errors" "$zod_errors errors found"
        grep "invalid_union\|invalid_type" "$OC_SERVE_LOG" | tail -3
    fi
fi

log "Test 11: ouija session_kill cleans up opencode session"
# Re-init MCP in case the session expired during the long wait
mcp_init "$BASE" >/dev/null 2>&1
kill_result=$(mcp_call_tool "$BASE" "session_kill" '{"name":"oc-e2e"}')
if echo "$kill_result" | grep -qi "killed\|removed"; then
    pass "ouija killed opencode session"
else
    fail "session_kill" "killed or removed" "$(echo "$kill_result" | head -c 200)"
fi

# Verify it's gone
sleep 1
remaining=$(api "$BASE" GET /api/status | jq -r '.sessions[] | select(.id == "oc-e2e") | .id')
if [ -z "$remaining" ]; then
    pass "oc-e2e session removed from ouija"
else
    fail "session cleanup" "session gone" "still exists"
fi

# ── Daemon logs ──────────────────────────────────────────────────
log "Daemon logs (last 20 lines):"
tail -20 /tmp/ouija-test/daemon.log 2>/dev/null || true

# ── Results ──────────────────────────────────────────────────────
print_results

# Cleanup
kill $DAEMON_PID 2>/dev/null || true
exit "$FAIL"
