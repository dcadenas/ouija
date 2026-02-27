#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

PORT=17880
BASE="http://127.0.0.1:$PORT"

# ── Setup: tmux server ──────────────────────────────────────────────
log "Starting tmux server"
tmux new-session -d -s test -x 200 -y 50

# Create panes that simulate "claude" by running a long-lived process named claude
FAKE_BIN=$(create_fake_claude)
export PATH="$FAKE_BIN:$PATH"

PANE_A=$(create_claude_pane "$FAKE_BIN")
PANE_B=$(create_claude_pane "$FAKE_BIN")

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
rm -rf /tmp/ouija-test
mkdir -p /tmp/ouija-test
echo '{"auto_register":false}' > /tmp/ouija-test/settings.json
DAEMON_PID=$(start_daemon $PORT "local" /tmp/ouija-test)
log "Daemon started (PID $DAEMON_PID, logs in /tmp/ouija-test/daemon.log)"

# ═══════════════════════════════════════════════════════════════════
# TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 1: Register with explicit pane"
result=$(api "$BASE" POST /api/register -d "{\"id\":\"sess-a\",\"pane\":\"$PANE_A\"}")
assert_contains "register returns id" "$result" '"registered":"sess-a"'
assert_contains "register returns pane" "$result" "\"pane\":\"$PANE_A\""
assert_eq "session count is 1" "$(session_count "$BASE")" "1"

log "Test 2: Register second session"
result=$(api "$BASE" POST /api/register -d "{\"id\":\"sess-b\",\"pane\":\"$PANE_B\"}")
assert_contains "register returns id" "$result" '"registered":"sess-b"'
assert_eq "session count is 2" "$(session_count "$BASE")" "2"

log "Test 3: Pane dedup — re-register same pane with new ID replaces old"
result=$(api "$BASE" POST /api/register -d "{\"id\":\"sess-a-renamed\",\"pane\":\"$PANE_A\"}")
assert_contains "dedup replaces old session" "$result" '"registered":"sess-a-renamed"'
assert_contains "reports replaced session" "$result" '"pane"'
ids=$(session_ids "$BASE")
assert_contains "new id present" "$ids" "sess-a-renamed"
assert_not_contains "old id gone" "$ids" "sess-a "
assert_eq "session count still 2" "$(session_count "$BASE")" "2"

log "Test 4: Rename via API"
result=$(api "$BASE" POST /api/rename -d '{"old_id":"sess-a-renamed","new_id":"sess-a2"}')
assert_contains "rename response" "$result" '"renamed"'
ids=$(session_ids "$BASE")
assert_contains "new name exists" "$ids" "sess-a2"
assert_not_contains "old name gone" "$ids" "sess-a-renamed"

log "Test 5: Remove via API"
api "$BASE" POST /api/register -d '{"id":"doomed"}' >/dev/null
count_before=$(session_count "$BASE")
result=$(api "$BASE" POST /api/remove -d '{"id":"doomed"}')
assert_contains "remove response" "$result" '"removed":"doomed"'
count_after=$(session_count "$BASE")
assert_eq "count decreased" "$count_after" "$((count_before - 1))"

log "Test 6: Remove non-existent returns error"
result=$(api "$BASE" POST /api/remove -d '{"id":"nope"}')
assert_contains "error for missing session" "$result" '"error"'

log "Test 7: Reaper removes session with dead pane"
api "$BASE" POST /api/register -d '{"id":"ghost","pane":"%99999"}' >/dev/null
# Note: reaper may remove dead-pane session before we can check count=3
log "  Waiting for reaper (max 10s)..."
for i in $(seq 1 10); do
    sleep 1
    count=$(session_count "$BASE")
    if [ "$count" = "2" ]; then break; fi
done
assert_eq "ghost reaped" "$(session_count "$BASE")" "2"
ids=$(session_ids "$BASE")
assert_not_contains "ghost gone" "$ids" "ghost"
assert_contains "sess-a2 survived" "$ids" "sess-a2"
assert_contains "sess-b survived" "$ids" "sess-b"

log "Test 8: Reaper keeps live claude pane"
# sess-a2 and sess-b have live panes running /tmp/claude
log "  Waiting 6s to confirm reaper doesn't kill live sessions..."
sleep 6
assert_eq "live sessions survive reaper" "$(session_count "$BASE")" "2"

log "Test 9: Reaper removes session whose pane runs non-claude process"
api "$BASE" POST /api/register -d "{\"id\":\"shell-session\",\"pane\":\"$PANE_SHELL\"}" >/dev/null
# Note: reaper may remove non-claude pane before we can check count=3
log "  Waiting for reaper..."
for i in $(seq 1 10); do
    sleep 1
    count=$(session_count "$BASE")
    if [ "$count" = "2" ]; then break; fi
done
assert_eq "shell pane reaped" "$(session_count "$BASE")" "2"
assert_not_contains "shell-session gone" "$(session_ids "$BASE")" "shell-session"

log "Test 10: Hook-based registration via ouija-register.sh"
mkdir -p /tmp/my-project
# Find the register hook script (local dev or Docker)
REGISTER_SCRIPT=$(find_script "ouija-register.sh")
# Enable auto-register
api "$BASE" POST /api/settings -d '{"auto_register":true}' >/dev/null
# Run the hook — it should register a session named "my-project"
HOOK_OUT=$(echo '{"source":"startup"}' | TMUX_PANE="$PANE_A" OUIJA_PORT=$PORT bash -c "cd /tmp/my-project && bash '$REGISTER_SCRIPT'" 2>&1)
assert_contains "hook registers session" "$HOOK_OUT" "Registered as my-project on the ouija mesh"
ids=$(session_ids "$BASE")
assert_contains "hook-registered session in list" "$ids" "my-project"
# Clean up
api "$BASE" POST /api/remove -d '{"id":"my-project"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/register -d "{\"id\":\"sess-a2\",\"pane\":\"$PANE_A\"}" >/dev/null
api "$BASE" POST /api/register -d "{\"id\":\"sess-b\",\"pane\":\"$PANE_B\"}" >/dev/null

log "Test 10b: Hook respects auto_register=false setting"
api "$BASE" POST /api/settings -d '{"auto_register":false}' >/dev/null
# Remove sess-a2 so we can try to re-register on its pane
api "$BASE" POST /api/remove -d '{"id":"sess-a2"}' >/dev/null 2>&1 || true
HOOK_OUT=$(echo '{"source":"startup"}' | TMUX_PANE="$PANE_A" OUIJA_PORT=$PORT bash -c "cd /tmp/my-project && bash '$REGISTER_SCRIPT'" 2>&1)
assert_eq "hook skips when auto_register=false" "$HOOK_OUT" ""
ids=$(session_ids "$BASE")
assert_not_contains "session not registered when disabled" "$ids" "my-project"
# Restore
api "$BASE" POST /api/settings -d '{"auto_register":true}' >/dev/null
api "$BASE" POST /api/register -d "{\"id\":\"sess-a2\",\"pane\":\"$PANE_A\"}" >/dev/null

log "Test 10b2: Register hook outputs mesh state"
# Set up: register a session with role and bulletin so the hook has peers to show
api "$BASE" POST /api/register -d "{\"id\":\"hook-peer\",\"pane\":\"$PANE_B\",\"role\":\"testing\",\"bulletin\":\"can help with tests\"}" >/dev/null
# Run the register hook script directly, simulating a SessionStart
SCRIPT_PATH=$(find_script "ouija-register.sh")
HOOK_OUTPUT=$(echo '{"source":"startup"}' | TMUX_PANE="$PANE_A" OUIJA_PORT=$PORT bash -c "cd /tmp/my-project && bash '$SCRIPT_PATH'" 2>&1)
assert_contains "hook output has registered message" "$HOOK_OUTPUT" "Registered as my-project on the ouija mesh"
assert_contains "hook output shows peer" "$HOOK_OUTPUT" "hook-peer"
assert_contains "hook output shows role" "$HOOK_OUTPUT" "testing"
assert_contains "hook output shows bulletin" "$HOOK_OUTPUT" "can help with tests"
# Clean up
api "$BASE" POST /api/remove -d '{"id":"my-project"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/remove -d '{"id":"hook-peer"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/register -d "{\"id\":\"sess-a2\",\"pane\":\"$PANE_A\"}" >/dev/null
api "$BASE" POST /api/register -d "{\"id\":\"sess-b\",\"pane\":\"$PANE_B\"}" >/dev/null

log "Test 10c: Register without pane"
result=$(api "$BASE" POST /api/register -d '{"id":"no-pane"}')
assert_contains "register without pane" "$result" '"registered":"no-pane"'
# Clean up
api "$BASE" POST /api/remove -d '{"id":"no-pane"}' >/dev/null

log "Test 10d: Register with bulletin"
result=$(api "$BASE" POST /api/register -d "{\"id\":\"bull-sess\",\"pane\":\"$PANE_A\",\"role\":\"tester\",\"bulletin\":\"need help with auth\"}")
assert_contains "register with bulletin" "$result" '"registered":"bull-sess"'
bull=$(session_field "$BASE" "bull-sess" "bulletin")
assert_eq "bulletin in status" "$bull" "need help with auth"
# Rename back for later tests
api "$BASE" POST /api/register -d "{\"id\":\"sess-a2\",\"pane\":\"$PANE_A\"}" >/dev/null

log "Test 10e: Update bulletin via API"
result=$(api "$BASE" POST /api/sessions/update -d '{"id":"sess-a2","bulletin":"offering Rust help"}')
assert_contains "update returns bulletin" "$result" '"bulletin":"offering Rust help"'
bull=$(session_field "$BASE" "sess-a2" "bulletin")
assert_eq "updated bulletin in status" "$bull" "offering Rust help"

log "Test 10f: MCP session_update sets bulletin"
result=$(mcp_call_tool "$BASE" "session_update" '{"id":"sess-b","bulletin":"can review PRs"}')
bull=$(session_field "$BASE" "sess-b" "bulletin")
assert_eq "MCP bulletin in status" "$bull" "can review PRs"

log "Test 11: Message injection into tmux pane"
result=$(api "$BASE" POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"hello from test\",\"expects_reply\":false}")
assert_contains "send delivered" "$result" '"status":"delivered"'
wait_for 5 bash -c "tmux capture-pane -t '$PANE_A' -p | grep -qF 'hello from test'"
pane_content=$(tmux capture-pane -t "$PANE_A" -p)
assert_contains "message appears in pane" "$pane_content" "hello from test"
assert_contains "no ? prefix" "$pane_content" "[from sess-b]:"

log "Test 11b: Long message injection (>200 chars, uses load-buffer)"
LONG_MSG="This is a long test message that exceeds 200 characters to exercise the inject_long code path which uses tmux load-buffer and paste-buffer instead of send-keys. It includes backticks \`like this\` and parentheses (like these) to test special character handling. Padding: AAAAAAAAAAAAA done."
result=$(api "$BASE" POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"$LONG_MSG\",\"expects_reply\":false}")
assert_contains "long send delivered" "$result" '"status":"delivered"'
wait_for 5 bash -c "tmux capture-pane -t '$PANE_A' -p -S -20 | grep -qF 'inject_long code path'"
pane_content=$(tmux capture-pane -t "$PANE_A" -p -S -20)
assert_contains "long message appears in pane" "$pane_content" "inject_long code path"

log "Test 11c: expects_reply=true adds ? prefix"
result=$(api "$BASE" POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"reply needed\",\"expects_reply\":true}")
assert_contains "expects_reply send delivered" "$result" '"status":"delivered"'
wait_for 5 bash -c "tmux capture-pane -t '$PANE_A' -p -S -20 | grep -qF '[from sess-b ?]:'"
pane_content=$(tmux capture-pane -t "$PANE_A" -p -S -20)
assert_contains "? prefix in pane" "$pane_content" "[from sess-b ?]:"

log "Test 11d: pending-replies endpoint includes message"
PANE_A_NUM="${PANE_A#%}"
result=$(api "$BASE" GET "/api/pane/${PANE_A_NUM}/pending-replies")
assert_contains "pending replies has count" "$result" '"count"'
assert_contains "pending replies has message field" "$result" '"message"'
assert_contains "pending replies message content" "$result" "reply needed"

log "Test 11e: DELETE pending reply clears it"
delete_status=$(curl -sf -o /dev/null -w '%{http_code}' -X DELETE "${BASE}/api/pane/${PANE_A_NUM}/pending-replies/sess-b" 2>/dev/null)
assert_eq "delete pending reply returns 200" "$delete_status" "200"
# Verify it's gone
result_after=$(api "$BASE" GET "/api/pane/${PANE_A_NUM}/pending-replies")
assert_contains "pending replies count is 0" "$result_after" '"count":0'

log "Test 11f: MCP clear_pending_reply tool"
# Create a new pending reply
api "$BASE" POST /api/send -d "{\"from\":\"sess-b\",\"to\":\"sess-a2\",\"message\":\"another reply needed\",\"expects_reply\":true}" >/dev/null
sleep 1
# Clear it via MCP
mcp_call_tool "$BASE" "clear_pending_reply" '{"session":"sess-a2","from":"sess-b"}' >/dev/null
result_after=$(api "$BASE" GET "/api/pane/${PANE_A_NUM}/pending-replies")
assert_contains "MCP clear leaves 0 pending" "$result_after" '"count":0'

log "Test 12: Send to non-existent session"
result=$(api "$BASE" POST /api/send -d '{"from":"sess-b","to":"nobody","message":"hi"}')
assert_contains "send error for missing" "$result" '"error"'

# ═══════════════════════════════════════════════════════════════════
# PERSISTENCE TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 12b: Sessions persisted to disk"
assert_eq "sessions.json exists" "$(test -f /tmp/ouija-test/sessions.json && echo yes)" "yes"
persisted_count=$(jq 'length' /tmp/ouija-test/sessions.json 2>/dev/null || echo 0)
assert_eq "persisted session count matches" "$persisted_count" "$(session_count "$BASE")"

log "Test 12c: Sessions restored after daemon restart"
# Disable auto-register so restarted daemon doesn't discover extra panes
api "$BASE" POST /api/settings -d '{"auto_register":false}' >/dev/null
count_before=$(session_count "$BASE")
ids_before=$(session_ids "$BASE")
log "  Sessions before restart: $ids_before ($count_before)"
# Kill and restart daemon
kill $DAEMON_PID 2>/dev/null || true
wait $DAEMON_PID 2>/dev/null || true
sleep 1
RUST_LOG=ouija=debug ouija start --port $PORT --data /tmp/ouija-test >/tmp/daemon-restart.log 2>&1 &
DAEMON_PID=$!
# Wait for HTTP to be ready
wait_for 10 curl -sf "$BASE/api/status" -o /dev/null
# Wait for async session restoration (runs in background spawn after HTTP is up)
for i in $(seq 1 50); do
    count_after=$(session_count "$BASE")
    if [ "$count_after" = "$count_before" ]; then break; fi
    sleep 0.2
done
count_after=$(session_count "$BASE")
ids_after=$(session_ids "$BASE")
log "  Sessions after restart: $ids_after ($count_after)"
assert_eq "session count preserved after restart" "$count_after" "$count_before"
# Check that each session ID survived
for sid in $ids_before; do
    assert_contains "session $sid survived restart" "$ids_after" "$sid"
done
# Restore auto_register
api "$BASE" POST /api/settings -d '{"auto_register":true}' >/dev/null

# ═══════════════════════════════════════════════════════════════════
# MCP PROTOCOL TESTS
# ═══════════════════════════════════════════════════════════════════

# Clean up API sessions first
api "$BASE" POST /api/remove -d '{"id":"sess-a2"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/remove -d '{"id":"sess-b"}' >/dev/null 2>&1 || true

log "Test 13: MCP initialize — server info returned"
mcp_init "$BASE" >/dev/null
init_result=$(grep '^data: {' /tmp/mcp-body | sed 's/^data: //')
assert_contains "init has server info" "$init_result" '"serverInfo"'
# stateful_mode=false means no session ID header — that's expected
if [ -n "$MCP_SESSION" ]; then
    log "  MCP session: $MCP_SESSION"
else
    log "  (stateless mode — no session ID, expected)"
fi

log "Test 14: MCP session_register without pane — auto-detects or errors"
result=$(mcp_call_tool "$BASE" "session_register" '{"id":"mcp-no-pane"}')
# If an unregistered claude pane exists, auto-detect succeeds; otherwise it errors.
if echo "$result" | grep -qF "registered as mcp-no-pane"; then
    pass "register without pane auto-detected"
    api "$BASE" POST /api/remove -d '{"id":"mcp-no-pane"}' >/dev/null 2>&1 || true
else
    assert_contains "error about pane" "$result" "pane is required"
    assert_contains "tells to run echo" "$result" 'echo $TMUX_PANE'
    assert_not_contains "not registered" "$(session_ids "$BASE")" "mcp-no-pane"
fi

log "Test 15: MCP session_register with pane — succeeds"
result=$(mcp_call_tool "$BASE" "session_register" "{\"id\":\"mcp-ok\",\"pane\":\"$PANE_A\"}")
assert_contains "registered via MCP" "$result" "registered as mcp-ok"
ids=$(session_ids "$BASE")
assert_contains "MCP session in list" "$ids" "mcp-ok"

log "Test 16: MCP session_list — returns sessions"
result=$(mcp_call_tool "$BASE" "session_list" '{}')
assert_contains "list contains session" "$result" "mcp-ok"

log "Test 17: MCP session_send — delivers message"
# Register a second session for messaging
api "$BASE" POST /api/register -d "{\"id\":\"mcp-target\",\"pane\":\"$PANE_B\"}" >/dev/null
result=$(mcp_call_tool "$BASE" "session_send" '{"from":"mcp-ok","to":"mcp-target","message":"hello via mcp","expects_reply":false}')
assert_contains "MCP send delivered" "$result" "delivered"
wait_for 5 bash -c "tmux capture-pane -t '$PANE_B' -p | grep -qF 'hello via mcp'"
pane_content=$(tmux capture-pane -t "$PANE_B" -p)
assert_contains "MCP message in pane" "$pane_content" "hello via mcp"

log "Test 18: MCP session_send to missing session — error"
result=$(mcp_call_tool "$BASE" "session_send" '{"from":"mcp-ok","to":"ghost","message":"hi","expects_reply":false}')
assert_contains "MCP send error" "$result" "not found"

log "Test 19: MCP pane dedup — re-register same pane via MCP replaces old"
result=$(mcp_call_tool "$BASE" "session_register" "{\"id\":\"mcp-renamed\",\"pane\":\"$PANE_A\"}")
assert_contains "MCP dedup replaces" "$result" "registered as mcp-renamed"
ids=$(session_ids "$BASE")
assert_contains "new MCP id present" "$ids" "mcp-renamed"
assert_not_contains "old MCP id gone" "$ids" "mcp-ok"

log "Test 19b: MCP re-register same ID updates metadata"
result=$(mcp_call_tool "$BASE" "session_register" "{\"id\":\"mcp-ok\",\"pane\":\"$PANE_A\",\"role\":\"updated-role\"}")
assert_contains "same-id re-register succeeds" "$result" "registered as mcp-ok"

# Cleanup MCP sessions
api "$BASE" POST /api/remove -d '{"id":"mcp-ok"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/remove -d '{"id":"mcp-target"}' >/dev/null 2>&1 || true

# ══════════════════════════════════════════════════════════════════════
# ── Scheduled Tasks ─────────────────────────────────────────────────
# ══════════════════════════════════════════════════════════════════════
log "Scheduled Tasks tests"

# T1: Create task via API
T1=$(api "$BASE" POST /api/tasks -d '{"name":"test-task","cron":"*/5 * * * *","target_session":"e2e-test","message":"hello from scheduler"}')
T1_ID=$(echo "$T1" | jq -r '.created // ""')
assert_contains "T1: create task returns id" "$T1" "created"

# T2: List tasks shows it
T2=$(api "$BASE" GET /api/tasks)
assert_contains "T2: list tasks contains task" "$T2" "$T1_ID"
assert_contains "T2: list tasks has name" "$T2" "test-task"

# T3: Trigger task manually (session doesn't exist, so it'll fail — but the trigger endpoint works)
T3=$(api "$BASE" POST /api/tasks/trigger -d "{\"id\":\"$T1_ID\"}")
assert_contains "T3: trigger task returns id" "$T3" "$T1_ID"

# T4: Task runs logged
T4=$(api "$BASE" GET /api/task-runs)
assert_contains "T4: task runs has entry" "$T4" "$T1_ID"

# T5: Disable task
T5=$(api "$BASE" POST /api/tasks/disable -d "{\"id\":\"$T1_ID\"}")
assert_contains "T5: disable task" "$T5" "$T1_ID"
T5_CHECK=$(api "$BASE" GET /api/tasks)
assert_contains "T5: task is disabled" "$T5_CHECK" "\"enabled\":false"

# T6: Enable task
T6=$(api "$BASE" POST /api/tasks/enable -d "{\"id\":\"$T1_ID\"}")
assert_contains "T6: enable task" "$T6" "$T1_ID"
T6_CHECK=$(api "$BASE" GET /api/tasks)
assert_contains "T6: task is enabled" "$T6_CHECK" "\"enabled\":true"

# T7: Delete task
T7=$(api "$BASE" DELETE /api/tasks -d "{\"id\":\"$T1_ID\"}")
assert_contains "T7: delete task" "$T7" "$T1_ID"

# T8: List tasks empty after delete
T8=$(api "$BASE" GET /api/tasks)
assert_not_contains "T8: task gone after delete" "$T8" "$T1_ID"

# ═══════════════════════════════════════════════════════════════════
# HUMAN SESSION TESTS (API level)
# ═══════════════════════════════════════════════════════════════════
log "Test H1: Add human session via API"
H1=$(api "$BASE" POST /api/humans -d '{"name":"daniel","npub":"npub1testfake","admin":true}')
assert_contains "H1: add human" "$H1" '"status":"added"'

log "Test H2: List humans shows the new entry"
H2=$(api "$BASE" GET /api/humans)
assert_contains "H2: list has daniel" "$H2" '"name":"daniel"'
assert_contains "H2: list has npub" "$H2" '"npub":"npub1testfake"'
assert_contains "H2: admin is true" "$H2" '"admin":true'

log "Test H3: Human appears in session list with origin human"
H3_ORIGIN=$(session_field "$BASE" "daniel" "origin")
assert_eq "H3: human session has origin human" "$H3_ORIGIN" "human"

log "Test H4: Duplicate human name rejected"
H4=$(api "$BASE" POST /api/humans -d '{"name":"daniel","npub":"npub1other"}')
assert_contains "H4: duplicate rejected" "$H4" '"error"'

log "Test H5: Remove human session"
H5=$(api "$BASE" DELETE /api/humans -d '{"name":"daniel"}')
assert_contains "H5: remove human" "$H5" '"status":"removed"'

log "Test H6: Human gone from session list"
H6_IDS=$(session_ids "$BASE")
assert_not_contains "H6: daniel gone" "$H6_IDS" "daniel"

log "Test H7: Human gone from humans list"
H7=$(api "$BASE" GET /api/humans)
assert_not_contains "H7: humans list empty" "$H7" '"name":"daniel"'

log "Test H8: Remove nonexistent human returns not found"
H8=$(api "$BASE" DELETE /api/humans -d '{"name":"ghost"}')
assert_contains "H8: not found" "$H8" '"error"'

# ═══════════════════════════════════════════════════════════════════
# SESSION LIFECYCLE TESTS (kill / start / restart)
# ═══════════════════════════════════════════════════════════════════

# Configure projects_dir for start/restart tests
api "$BASE" POST /api/settings -d '{"projects_dir":"/tmp/projects"}' >/dev/null

log "Test L1: Start session via REST API"
L1=$(api "$BASE" POST /api/sessions/start -d '{"name":"lifecycle-test"}')
assert_contains "L1: start returns pane" "$L1" "pane"
assert_contains "L1: start creates dir" "$L1" "/tmp/projects/lifecycle-test"
# Verify session registered
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'lifecycle-test'"
L1_IDS=$(session_ids "$BASE")
assert_contains "L1: session registered" "$L1_IDS" "lifecycle-test"
# Verify directory was created
assert_eq "L1: project dir created" "$(test -d /tmp/projects/lifecycle-test && echo yes)" "yes"
# Verify tmux session exists
tmux_sessions=$(tmux list-sessions -F '#{session_name}' 2>/dev/null)
assert_contains "L1: tmux session created" "$tmux_sessions" "lifecycle-test"

log "Test L2: Start duplicate session fails"
L2=$(api "$BASE" POST /api/sessions/start -d '{"name":"lifecycle-test"}')
assert_contains "L2: duplicate rejected" "$L2" "already exists"

log "Test L3: Kill session via REST API"
L3=$(api "$BASE" POST /api/sessions/kill -d '{"name":"lifecycle-test"}')
assert_contains "L3: kill response" "$L3" "removed"
wait_for 5 bash -c "! session_ids '$BASE' | grep -qF 'lifecycle-test'"
# Verify session removed from daemon
L3_IDS=$(session_ids "$BASE")
assert_not_contains "L3: session removed" "$L3_IDS" "lifecycle-test"

log "Test L4: Kill non-existent session"
L4=$(api "$BASE" POST /api/sessions/kill -d '{"name":"no-such-session"}')
assert_contains "L4: not found" "$L4" "not found"

log "Test L5: Restart session via REST API (creates new if not running)"
L5=$(api "$BASE" POST /api/sessions/restart -d '{"name":"restart-test"}')
assert_contains "L5: restart response has pane" "$L5" "pane"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'restart-test'"
L5_IDS=$(session_ids "$BASE")
assert_contains "L5: restarted session registered" "$L5_IDS" "restart-test"

log "Test L6: Restart existing session (kill + start)"
L6=$(api "$BASE" POST /api/sessions/restart -d '{"name":"restart-test"}')
assert_contains "L6: restart response" "$L6" "restarted"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'restart-test'"
L6_IDS=$(session_ids "$BASE")
assert_contains "L6: session still registered after restart" "$L6_IDS" "restart-test"

log "Test L6b: Metadata preserved after restart"
# Register a session with rich metadata, restart, verify metadata survives
# Use a pane with fake claude process so pane_alive (has_claude_descendant) passes
L6B_PANE=$(create_claude_pane "$FAKE_BIN")
sleep 1
api "$BASE" POST /api/register -d "{\"id\":\"meta-restart\",\"pane\":\"$L6B_PANE\",\"vim_mode\":true,\"role\":\"backend\",\"project_dir\":\"/tmp/meta-test\"}" >/dev/null
sleep 1
L6B_ROLE_BEFORE=$(session_field "$BASE" "meta-restart" "role")
assert_eq "L6b: role set before restart" "$L6B_ROLE_BEFORE" "backend"
# Restart and check metadata survived
api "$BASE" POST /api/sessions/restart -d '{"name":"meta-restart"}' >/dev/null
sleep 2
L6B_VIM=$(session_field "$BASE" "meta-restart" "vim_mode")
L6B_ROLE=$(session_field "$BASE" "meta-restart" "role")
L6B_DIR=$(session_field "$BASE" "meta-restart" "project_dir")
assert_eq "L6b: vim_mode preserved" "$L6B_VIM" "true"
assert_eq "L6b: role preserved" "$L6B_ROLE" "backend"
assert_eq "L6b: project_dir preserved" "$L6B_DIR" "/tmp/meta-test"

log "Test L6c: Pane ID changes after restart"
L6C_PANE_BEFORE=$(session_field "$BASE" "meta-restart" "pane")
api "$BASE" POST /api/sessions/restart -d '{"name":"meta-restart"}' >/dev/null
sleep 2
L6C_PANE_AFTER=$(session_field "$BASE" "meta-restart" "pane")
if [ -n "$L6C_PANE_BEFORE" ] && [ -n "$L6C_PANE_AFTER" ] && [ "$L6C_PANE_BEFORE" != "$L6C_PANE_AFTER" ]; then
    pass "L6c: pane changed after restart ($L6C_PANE_BEFORE -> $L6C_PANE_AFTER)"
else
    fail "L6c: pane should change after restart" "different pane" "before=$L6C_PANE_BEFORE after=$L6C_PANE_AFTER"
fi

log "Test L6d: Restart pane runs correct command"
sleep 2
L6D_PANE=$(session_field "$BASE" "meta-restart" "pane")
L6D_CONTENT=$(tmux capture-pane -t "$L6D_PANE" -p 2>/dev/null || echo "")
if echo "$L6D_CONTENT" | grep -qE '(--resume|--continue)'; then
    pass "L6d: restart pane has --resume or --continue flag"
else
    fail "L6d: restart pane should have --resume or --continue" "--resume or --continue" "$L6D_CONTENT"
fi

log "Test L6e: Reaper grace period — session survives reaper cycle after restart"
api "$BASE" POST /api/sessions/restart -d '{"name":"meta-restart"}' >/dev/null
sleep 6
L6E_IDS=$(session_ids "$BASE")
assert_contains "L6e: session survives reaper after restart" "$L6E_IDS" "meta-restart"

log "Test L6f: Fresh restart via API"
L6F=$(api "$BASE" POST /api/sessions/restart -d '{"name":"meta-restart","fresh":true}')
assert_contains "L6f: fresh restart response" "$L6F" "restarted"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'meta-restart'"
L6F_IDS=$(session_ids "$BASE")
assert_contains "L6f: session survived fresh restart" "$L6F_IDS" "meta-restart"

# ═══════════════════════════════════════════════════════════════════
# (Mesh context embedded resources were removed in favor of the session-diff hook)

# Clean up lifecycle sessions
api "$BASE" POST /api/sessions/kill -d '{"name":"meta-restart"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/sessions/kill -d '{"name":"restart-test"}' >/dev/null 2>&1 || true

# --- MCP lifecycle tests ---

log "Test L7: MCP session_start"
L7=$(mcp_call_tool "$BASE" "session_start" '{"name":"mcp-lifecycle"}')
assert_contains "L7: MCP start has pane" "$L7" "pane"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'mcp-lifecycle'"
L7_IDS=$(session_ids "$BASE")
assert_contains "L7: MCP started session registered" "$L7_IDS" "mcp-lifecycle"

log "Test L8: MCP session_kill"
L8=$(mcp_call_tool "$BASE" "session_kill" '{"name":"mcp-lifecycle"}')
assert_contains "L8: MCP kill removed" "$L8" "removed"
wait_for 5 bash -c "! session_ids '$BASE' | grep -qF 'mcp-lifecycle'"
L8_IDS=$(session_ids "$BASE")
assert_not_contains "L8: MCP killed session gone" "$L8_IDS" "mcp-lifecycle"

log "Test L9: MCP session_restart"
# Start first, then restart
mcp_call_tool "$BASE" "session_start" '{"name":"mcp-restart"}' >/dev/null
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'mcp-restart'"
L9=$(mcp_call_tool "$BASE" "session_restart" '{"name":"mcp-restart"}')
assert_contains "L9: MCP restart response" "$L9" "restarted"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'mcp-restart'"
L9_IDS=$(session_ids "$BASE")
assert_contains "L9: MCP restarted session registered" "$L9_IDS" "mcp-restart"

log "Test L10: MCP session_restart with fresh=true"
L10=$(mcp_call_tool "$BASE" "session_restart" '{"name":"mcp-restart","fresh":true}')
assert_contains "L10: MCP fresh restart response" "$L10" "restarted"
wait_for 5 bash -c "session_ids '$BASE' | grep -qF 'mcp-restart'"
L10_IDS=$(session_ids "$BASE")
assert_contains "L10: MCP fresh restarted session registered" "$L10_IDS" "mcp-restart"

log "Test L11: Task creation with on_fire new_session"
L11=$(api "$BASE" POST /api/tasks -d '{"name":"fresh-task","cron":"0 0 * * *","target_session":"mcp-restart","message":"test","on_fire":{"mode":"new_session"}}')
assert_contains "L11: create new_session task returns id" "$L11" "created"
L11_ID=$(echo "$L11" | jq -r '.created')
L11_TASK=$(api "$BASE" GET "/api/tasks")
L11_MODE=$(echo "$L11_TASK" | jq -r --arg id "$L11_ID" '.tasks[] | select(.id == $id) | .on_fire.mode')
assert_eq "L11: task on_fire mode is new_session" "$L11_MODE" "new_session"
api "$BASE" DELETE "/api/tasks/$L11_ID" >/dev/null

log "Test L12: Task creation with persistent_worktree"
L12=$(api "$BASE" POST /api/tasks -d '{"name":"wt-task","cron":"0 0 * * *","target_session":"mcp-restart","message":"test","on_fire":{"mode":"persistent_worktree"}}')
assert_contains "L12: create persistent worktree task returns id" "$L12" "created"
L12_ID=$(echo "$L12" | jq -r '.created')
L12_TASK=$(api "$BASE" GET "/api/tasks")
L12_MODE=$(echo "$L12_TASK" | jq -r --arg id "$L12_ID" '.tasks[] | select(.id == $id) | .on_fire.mode')
assert_eq "L12: task on_fire mode is persistent_worktree" "$L12_MODE" "persistent_worktree"
api "$BASE" DELETE "/api/tasks/$L12_ID" >/dev/null

log "Test L13: Task creation with disposable_worktree"
L13=$(api "$BASE" POST /api/tasks -d '{"name":"pf-task","cron":"0 0 * * *","target_session":"mcp-restart","message":"test","on_fire":{"mode":"disposable_worktree"}}')
assert_contains "L13: create disposable worktree task returns id" "$L13" "created"
L13_ID=$(echo "$L13" | jq -r '.created')
L13_TASK=$(api "$BASE" GET "/api/tasks")
L13_MODE=$(echo "$L13_TASK" | jq -r --arg id "$L13_ID" '.tasks[] | select(.id == $id) | .on_fire.mode')
assert_eq "L13: task on_fire mode is disposable_worktree" "$L13_MODE" "disposable_worktree"
api "$BASE" DELETE "/api/tasks/$L13_ID" >/dev/null

log "Test L15: Auto-worktree when directory conflicts"
# Register a session with project_dir that will conflict with a new session_start
mkdir -p /tmp/projects/auto-wt-test
api "$BASE" POST /api/register -d "{\"id\":\"existing-sess\",\"pane\":\"$PANE_A\",\"project_dir\":\"/tmp/projects/auto-wt-test\"}" >/dev/null
L15=$(api "$BASE" POST /api/sessions/start -d '{"name":"auto-wt-test"}')
assert_contains "L15: start succeeds" "$L15" "started"
assert_contains "L15: auto-worktree noted" "$L15" "auto-enabled"
L15_STATUS=$(api "$BASE" GET /api/status)
L15_WT=$(echo "$L15_STATUS" | jq -r '.sessions[] | select(.id == "auto-wt-test") | .worktree')
assert_eq "L15: auto-worktree session has worktree=true" "$L15_WT" "true"
api "$BASE" POST /api/sessions/kill -d '{"name":"auto-wt-test"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/remove -d '{"id":"existing-sess"}' >/dev/null 2>&1 || true

log "Test L14: Start session with worktree=true"
L14=$(api "$BASE" POST /api/sessions/start -d '{"name":"wt-sess","worktree":true}')
assert_contains "L14: start worktree session" "$L14" "started"
L14_STATUS=$(api "$BASE" GET /api/status)
L14_WT=$(echo "$L14_STATUS" | jq -r '.sessions[] | select(.id == "wt-sess") | .worktree')
assert_eq "L14: session has worktree=true in metadata" "$L14_WT" "true"
api "$BASE" POST /api/sessions/kill -d '{"name":"wt-sess"}' >/dev/null 2>&1 || true

# Clean up
api "$BASE" POST /api/sessions/kill -d '{"name":"mcp-restart"}' >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════
# IDLE DETECTION TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 20: Idle detection via /stopped and /active"
# Set idle timeout to 2 seconds for fast test
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":2}' >/dev/null
# Register fresh session
api "$BASE" POST /api/register -d "{\"id\":\"idle-test\",\"pane\":\"$PANE_A\"}" >/dev/null 2>&1 || true
PANE_A_NUM="${PANE_A#%}"
# Signal stopped — should start idle timer
stopped_status=$(curl -sf -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/pane/${PANE_A_NUM}/stopped" 2>/dev/null)
assert_eq "stopped returns 200" "$stopped_status" "200"
# Signal active — should cancel idle timer
active_status=$(curl -sf -o /dev/null -w '%{http_code}' -X POST "${BASE}/api/pane/${PANE_A_NUM}/active" 2>/dev/null)
assert_eq "active returns 200" "$active_status" "200"
pass "idle detection endpoints respond"
# Clean up
api "$BASE" POST /api/remove -d '{"id":"idle-test"}' >/dev/null 2>&1 || true
# Restore idle timeout
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":60}' >/dev/null

log "Test 20a: Idle reminder re-injects unanswered pending replies"
# Set idle timeout to 2 seconds for fast test
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":2}' >/dev/null
# Register fresh session on PANE_A
api "$BASE" POST /api/remove -d '{"id":"reminder-test"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/register -d "{\"id\":\"reminder-test\",\"pane\":\"$PANE_A\"}" >/dev/null
PANE_A_NUM="${PANE_A#%}"
# Send a ? message to create a pending reply
api "$BASE" POST /api/send -d "{\"from\":\"asker\",\"to\":\"reminder-test\",\"message\":\"do you have the answer?\",\"expects_reply\":true}" >/dev/null
sleep 0.5
# Verify pending reply exists
result=$(api "$BASE" GET "/api/pane/${PANE_A_NUM}/pending-replies")
assert_contains "pending reply tracked" "$result" '"count":1'
# Clear the pane so we can detect the reminder injection
tmux send-keys -t "$PANE_A" "clear" Enter
sleep 0.5
# Signal stopped — starts idle timer (2s)
curl -sf -X POST "${BASE}/api/pane/${PANE_A_NUM}/stopped" >/dev/null 2>&1
# Wait for idle timeout + reminder injection
wait_for 8 bash -c "tmux capture-pane -t '$PANE_A' -p -S -20 | grep -qF 'unanswered question from asker'"
pane_content=$(tmux capture-pane -t "$PANE_A" -p -S -20)
assert_contains "reminder injected on idle" "$pane_content" "unanswered question from asker"
assert_contains "reminder includes session_send hint" "$pane_content" "session_send"
assert_not_contains "no ouija prefix in reminder" "$pane_content" "[ouija]"
# Second stopped should NOT re-inject (reminded flag is set)
# Count injections by capturing pane content before and after second idle cycle
pre_count=$(tmux capture-pane -t "$PANE_A" -p -S -50 | grep -c "unanswered question from asker" || true)
curl -sf -X POST "${BASE}/api/pane/${PANE_A_NUM}/stopped" >/dev/null 2>&1
sleep 4
post_count=$(tmux capture-pane -t "$PANE_A" -p -S -50 | grep -c "unanswered question from asker" || true)
assert_eq "no duplicate reminder" "$post_count" "$pre_count"
# Clean up
api "$BASE" POST /api/remove -d '{"id":"reminder-test"}' >/dev/null 2>&1 || true
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":60}' >/dev/null

log "Test 20b: idle_timeout_secs persisted in settings"
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":120}' >/dev/null
settings_json=$(cat /tmp/ouija-test/settings.json 2>/dev/null)
assert_contains "idle timeout saved to disk" "$settings_json" '"idle_timeout_secs": 120'
# Restore
api "$BASE" POST /api/settings -d '{"idle_timeout_secs":60}' >/dev/null

# ── Daemon logs ──────────────────────────────────────────────────────
log "Daemon logs:"
cat /tmp/ouija-test/daemon.log 2>/dev/null || true

# ── Results ──────────────────────────────────────────────────────────
print_results

# Cleanup
kill $DAEMON_PID 2>/dev/null || true
exit "$FAIL"
