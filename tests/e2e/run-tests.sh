#!/bin/bash
set -euo pipefail

# ── Colours ──────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASS=0
FAIL=0
PORT=7880
BASE="http://127.0.0.1:$PORT"

# ── Helpers ──────────────────────────────────────────────────────────
log()  { echo -e "${YELLOW}>>> $*${NC}"; }
pass() { echo -e "  ${GREEN}PASS${NC}: $1"; PASS=$((PASS + 1)); }
fail() { echo -e "  ${RED}FAIL${NC}: $1 (expected: $2, got: $3)"; FAIL=$((FAIL + 1)); }

assert_eq() {
    local desc="$1" actual="$2" expected="$3"
    if [ "$actual" = "$expected" ]; then pass "$desc"; else fail "$desc" "$expected" "$actual"; fi
}

assert_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if echo "$haystack" | grep -qF "$needle"; then pass "$desc"; else fail "$desc" "contains '$needle'" "$haystack"; fi
}

assert_not_contains() {
    local desc="$1" haystack="$2" needle="$3"
    if ! echo "$haystack" | grep -qF "$needle"; then pass "$desc"; else fail "$desc" "not contains '$needle'" "$haystack"; fi
}

api() {
    local method="$1" path="$2"
    shift 2
    curl -sf -X "$method" "$BASE$path" \
        -H 'Content-Type: application/json' "$@" 2>/dev/null || echo '{"error":"curl failed"}'
}

session_ids() { api GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions']))" 2>/dev/null; }
session_count() { api GET /api/status | python3 -c "import sys,json; print(len(json.load(sys.stdin)['sessions']))" 2>/dev/null; }

# MCP JSON-RPC helpers
# The MCP streamable HTTP transport returns SSE (text/event-stream).
# We extract JSON from "data: {..." lines and session ID from headers.
MCP_ID=0
MCP_SESSION=""

mcp_init() {
    MCP_ID=$((MCP_ID + 1))
    # Step 1: Send initialize request (SSE keeps connection open, timeout kills it)
    timeout 5 curl -s -D /tmp/mcp-headers -X POST "$BASE/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1.0\"},\"protocolVersion\":\"2025-03-26\"},\"id\":$MCP_ID}" \
        >/tmp/mcp-body 2>/dev/null || true
    MCP_SESSION=$(sed -n 's/^mcp-session-id: *//Ip' /tmp/mcp-headers | tr -d '\r\n')

    # Step 2: Send notifications/initialized (required by MCP before tool calls)
    timeout 2 curl -s -X POST "$BASE/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $MCP_SESSION" \
        -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        >/dev/null 2>&1 || true

    # Extract JSON from SSE data: lines
    { grep '^data: {' /tmp/mcp-body || true; } | sed 's/^data: //'
}

mcp_call_tool() {
    local tool="$1" args="$2"
    MCP_ID=$((MCP_ID + 1))
    timeout 5 curl -s -X POST "$BASE/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $MCP_SESSION" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"$tool\",\"arguments\":$args},\"id\":$MCP_ID}" \
        >/tmp/mcp-tool-body 2>/dev/null || true
    { grep '^data: {' /tmp/mcp-tool-body || true; } | sed 's/^data: //'
}

# ── Setup: tmux server ──────────────────────────────────────────────
log "Starting tmux server"
tmux new-session -d -s test -x 200 -y 50

# Create panes that simulate "claude" by running a long-lived process named claude
# We use a simple trick: copy /bin/sleep to /tmp/claude so pane_current_command shows "claude"
cp /bin/sleep /tmp/claude

tmux new-window -t test
PANE_A=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_A" '/tmp/claude 3600' Enter

tmux new-window -t test
PANE_B=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_B" '/tmp/claude 3600' Enter

# A pane running a normal shell (for reaper tests)
tmux new-window -t test
PANE_SHELL=$(tmux display-message -t test -p '#{pane_id}')
# Don't run anything special — default shell (bash)

# Wait for processes to start
sleep 1

log "Panes: claude-A=$PANE_A  claude-B=$PANE_B  shell=$PANE_SHELL"
tmux list-panes -a -F '#{pane_id} #{pane_current_command}'

# ── Setup: ouija daemon ─────────────────────────────────────────────
log "Starting ouija daemon"
RUST_LOG=ouija=debug ouija start --port $PORT --data /tmp/ouija-test >/tmp/daemon.log 2>&1 &
DAEMON_PID=$!

# Wait for daemon
for i in $(seq 1 50); do
    if curl -sf "$BASE/api/status" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
log "Daemon started (PID $DAEMON_PID, logs in /tmp/daemon.log)"

# ═══════════════════════════════════════════════════════════════════
# TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 1: Register with explicit pane"
result=$(api POST /api/register -d "{\"id\":\"sess-a\",\"pane\":\"$PANE_A\"}")
assert_contains "register returns id" "$result" '"registered":"sess-a"'
assert_contains "register returns pane" "$result" "\"pane\":\"$PANE_A\""
assert_eq "session count is 1" "$(session_count)" "1"

log "Test 2: Register second session"
result=$(api POST /api/register -d "{\"id\":\"sess-b\",\"pane\":\"$PANE_B\"}")
assert_contains "register returns id" "$result" '"registered":"sess-b"'
assert_eq "session count is 2" "$(session_count)" "2"

log "Test 3: Pane dedup — re-register same pane with new ID is rejected"
result=$(curl -s -X POST "$BASE/api/register" -H 'Content-Type: application/json' -d "{\"id\":\"sess-a-renamed\",\"pane\":\"$PANE_A\"}" 2>/dev/null)
assert_contains "dedup returns conflict" "$result" "already registered"
assert_contains "reports existing name" "$result" "sess-a"
ids=$(session_ids)
assert_contains "original id preserved" "$ids" "sess-a"
assert_eq "session count still 2" "$(session_count)" "2"

log "Test 4: Rename via API"
result=$(api POST /api/rename -d '{"old_id":"sess-a","new_id":"sess-a2"}')
assert_contains "rename response" "$result" '"renamed"'
ids=$(session_ids)
assert_contains "new name exists" "$ids" "sess-a2"
assert_not_contains "old name gone" "$ids" "sess-a-renamed"

log "Test 5: Remove via API"
api POST /api/register -d '{"id":"doomed","pane":"%99998"}' >/dev/null
count_before=$(session_count)
result=$(api POST /api/remove -d '{"id":"doomed"}')
assert_contains "remove response" "$result" '"removed":"doomed"'
count_after=$(session_count)
assert_eq "count decreased" "$count_after" "$((count_before - 1))"

log "Test 6: Remove non-existent returns error"
result=$(api POST /api/remove -d '{"id":"nope"}')
assert_contains "error for missing session" "$result" '"error"'

log "Test 7: Reaper removes session with dead pane"
api POST /api/register -d '{"id":"ghost","pane":"%99999"}' >/dev/null
assert_eq "ghost registered" "$(session_count)" "3"
log "  Waiting for reaper (max 35s)..."
for i in $(seq 1 35); do
    sleep 1
    count=$(session_count)
    if [ "$count" = "2" ]; then break; fi
done
assert_eq "ghost reaped" "$(session_count)" "2"
ids=$(session_ids)
assert_not_contains "ghost gone" "$ids" "ghost"
assert_contains "sess-a2 survived" "$ids" "sess-a2"
assert_contains "sess-b survived" "$ids" "sess-b"

log "Test 8: Reaper keeps live claude pane"
# sess-a2 and sess-b have live panes running /tmp/claude
log "  Waiting 35s to confirm reaper doesn't kill live sessions..."
sleep 35
assert_eq "live sessions survive reaper" "$(session_count)" "2"

log "Test 9: Reaper removes session whose pane runs non-claude process"
api POST /api/register -d "{\"id\":\"shell-session\",\"pane\":\"$PANE_SHELL\"}" >/dev/null
assert_eq "shell session registered" "$(session_count)" "3"
log "  Waiting for reaper..."
for i in $(seq 1 35); do
    sleep 1
    count=$(session_count)
    if [ "$count" = "2" ]; then break; fi
done
assert_eq "shell pane reaped" "$(session_count)" "2"
assert_not_contains "shell-session gone" "$(session_ids)" "shell-session"

log "Test 10: Register without pane"
result=$(api POST /api/register -d '{"id":"no-pane"}')
assert_contains "register without pane" "$result" '"registered":"no-pane"'
# Clean up
api POST /api/remove -d '{"id":"no-pane"}' >/dev/null

log "Test 11: Message injection into tmux pane"
result=$(api POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"hello from test\"}")
assert_contains "send delivered" "$result" '"status":"delivered"'
sleep 1
pane_content=$(tmux capture-pane -t "$PANE_A" -p)
assert_contains "message appears in pane" "$pane_content" "hello from test"

log "Test 11b: Long message injection (>200 chars, uses load-buffer)"
LONG_MSG="This is a long test message that exceeds 200 characters to exercise the inject_long code path which uses tmux load-buffer and paste-buffer instead of send-keys. It includes backticks \`like this\` and parentheses (like these) to test special character handling. Padding: AAAAAAAAAAAAA done."
result=$(api POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"$LONG_MSG\"}")
assert_contains "long send delivered" "$result" '"status":"delivered"'
sleep 1
pane_content=$(tmux capture-pane -t "$PANE_A" -p -S -20)
assert_contains "long message appears in pane" "$pane_content" "inject_long code path"

log "Test 12: Send to non-existent session"
result=$(api POST /api/send -d '{"from":"sess-b","to":"nobody","message":"hi"}')
assert_contains "send error for missing" "$result" '"error"'

# ═══════════════════════════════════════════════════════════════════
# PERSISTENCE TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 12b: Sessions persisted to disk"
assert_eq "sessions.json exists" "$(test -f /tmp/ouija-test/sessions.json && echo yes)" "yes"
persisted_count=$(python3 -c "import json; print(len(json.load(open('/tmp/ouija-test/sessions.json'))))" 2>/dev/null || echo 0)
assert_eq "persisted session count matches" "$persisted_count" "$(session_count)"

log "Test 12c: Sessions restored after daemon restart"
count_before=$(session_count)
ids_before=$(session_ids)
log "  Sessions before restart: $ids_before ($count_before)"
# Kill and restart daemon
kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true
sleep 1
RUST_LOG=ouija=debug ouija start --port $PORT --data /tmp/ouija-test >/tmp/daemon-restart.log 2>&1 &
DAEMON_PID=$!
# Wait for HTTP to be ready
for i in $(seq 1 50); do
    if curl -sf "$BASE/api/status" >/dev/null 2>&1; then break; fi
    sleep 0.1
done
# Wait for async session restoration (runs in background spawn after HTTP is up)
for i in $(seq 1 50); do
    count_after=$(session_count)
    if [ "$count_after" = "$count_before" ]; then break; fi
    sleep 0.2
done
count_after=$(session_count)
ids_after=$(session_ids)
log "  Sessions after restart: $ids_after ($count_after)"
assert_eq "session count preserved after restart" "$count_after" "$count_before"
# Check that each session ID survived
for sid in $ids_before; do
    assert_contains "session $sid survived restart" "$ids_after" "$sid"
done

# ═══════════════════════════════════════════════════════════════════
# MCP PROTOCOL TESTS
# ═══════════════════════════════════════════════════════════════════

# Clean up API sessions first
api POST /api/remove -d '{"id":"sess-a2"}' >/dev/null 2>&1 || true
api POST /api/remove -d '{"id":"sess-b"}' >/dev/null 2>&1 || true

log "Test 13: MCP initialize — server info returned"
mcp_init >/dev/null
init_result=$(grep '^data: {' /tmp/mcp-body | sed 's/^data: //')
assert_contains "init has server info" "$init_result" '"serverInfo"'
# stateful_mode=false means no session ID header — that's expected
if [ -n "$MCP_SESSION" ]; then
    log "  MCP session: $MCP_SESSION"
else
    log "  (stateless mode — no session ID, expected)"
fi

log "Test 14: MCP peer_register without pane — returns error"
result=$(mcp_call_tool "peer_register" '{"id":"mcp-no-pane"}')
assert_contains "error about pane" "$result" "pane is required"
assert_contains "tells to run echo" "$result" 'echo $TMUX_PANE'
# Verify it did NOT register
assert_not_contains "not registered" "$(session_ids)" "mcp-no-pane"

log "Test 15: MCP peer_register with pane — succeeds"
result=$(mcp_call_tool "peer_register" "{\"id\":\"mcp-ok\",\"pane\":\"$PANE_A\"}")
assert_contains "registered via MCP" "$result" "registered as mcp-ok"
ids=$(session_ids)
assert_contains "MCP session in list" "$ids" "mcp-ok"

log "Test 16: MCP peer_list — returns sessions"
result=$(mcp_call_tool "peer_list" '{}')
assert_contains "list contains session" "$result" "mcp-ok"

log "Test 17: MCP peer_send — delivers message"
# Register a second session for messaging
api POST /api/register -d "{\"id\":\"mcp-target\",\"pane\":\"$PANE_B\"}" >/dev/null
result=$(mcp_call_tool "peer_send" '{"from":"mcp-ok","to":"mcp-target","message":"hello via mcp"}')
assert_contains "MCP send delivered" "$result" "delivered"
sleep 1
pane_content=$(tmux capture-pane -t "$PANE_B" -p)
assert_contains "MCP message in pane" "$pane_content" "hello via mcp"

log "Test 18: MCP peer_send to missing session — error"
result=$(mcp_call_tool "peer_send" '{"from":"mcp-ok","to":"ghost","message":"hi"}')
assert_contains "MCP send error" "$result" "not found"

log "Test 19: MCP pane dedup — re-register same pane via MCP is rejected"
result=$(mcp_call_tool "peer_register" "{\"id\":\"mcp-renamed\",\"pane\":\"$PANE_A\"}")
assert_contains "MCP dedup error" "$result" "already registered"
assert_contains "MCP reports existing name" "$result" "mcp-ok"
ids=$(session_ids)
assert_contains "original MCP id preserved" "$ids" "mcp-ok"
assert_not_contains "rejected id not created" "$ids" "mcp-renamed"

log "Test 19b: MCP re-register same ID updates metadata"
result=$(mcp_call_tool "peer_register" "{\"id\":\"mcp-ok\",\"pane\":\"$PANE_A\",\"role\":\"updated-role\"}")
assert_contains "same-id re-register succeeds" "$result" "registered as mcp-ok"

# Cleanup MCP sessions
api POST /api/remove -d '{"id":"mcp-ok"}' >/dev/null 2>&1 || true
api POST /api/remove -d '{"id":"mcp-target"}' >/dev/null 2>&1 || true

# ══════════════════════════════════════════════════════════════════════
# ── Scheduled Tasks ─────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
log "Scheduled Tasks tests"

# T1: Create task via API
T1=$(api POST /api/tasks -d '{"name":"test-task","cron":"*/5 * * * *","target_session":"e2e-test","message":"hello from scheduler"}')
T1_ID=$(echo "$T1" | python3 -c "import sys,json; print(json.load(sys.stdin).get('created',''))" 2>/dev/null)
assert_contains "T1: create task returns id" "$T1" "created"

# T2: List tasks shows it
T2=$(api GET /api/tasks)
assert_contains "T2: list tasks contains task" "$T2" "$T1_ID"
assert_contains "T2: list tasks has name" "$T2" "test-task"

# T3: Trigger task manually (session doesn't exist, so it'll fail — but the trigger endpoint works)
T3=$(api POST /api/tasks/trigger -d "{\"id\":\"$T1_ID\"}")
assert_contains "T3: trigger task returns id" "$T3" "$T1_ID"

# T4: Task runs logged
T4=$(api GET /api/task-runs)
assert_contains "T4: task runs has entry" "$T4" "$T1_ID"

# T5: Disable task
T5=$(api POST /api/tasks/disable -d "{\"id\":\"$T1_ID\"}")
assert_contains "T5: disable task" "$T5" "$T1_ID"
T5_CHECK=$(api GET /api/tasks)
assert_contains "T5: task is disabled" "$T5_CHECK" "\"enabled\":false"

# T6: Enable task
T6=$(api POST /api/tasks/enable -d "{\"id\":\"$T1_ID\"}")
assert_contains "T6: enable task" "$T6" "$T1_ID"
T6_CHECK=$(api GET /api/tasks)
assert_contains "T6: task is enabled" "$T6_CHECK" "\"enabled\":true"

# T7: Delete task
T7=$(api DELETE /api/tasks -d "{\"id\":\"$T1_ID\"}")
assert_contains "T7: delete task" "$T7" "$T1_ID"

# T8: List tasks empty after delete
T8=$(api GET /api/tasks)
assert_not_contains "T8: task gone after delete" "$T8" "$T1_ID"

# ── Daemon logs ──────────────────────────────────────────────────────
log "Daemon logs:"
cat /tmp/daemon.log 2>/dev/null || true

# ── Results ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════"
echo -e "Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}"
if [ "$FAIL" -eq 0 ]; then
    echo -e "${GREEN}ALL TESTS PASSED${NC}"
else
    echo -e "${RED}SOME TESTS FAILED${NC}"
fi
echo "════════════════════════════════════════════"

# Cleanup
kill $DAEMON_PID 2>/dev/null || true
exit "$FAIL"
