#!/bin/bash
set -euo pipefail

# ── Colours ──────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASS=0
FAIL=0
PORT_A=7880
PORT_B=7881
RELAY_URL="ws://127.0.0.1:8080"
BASE_A="http://127.0.0.1:$PORT_A"
BASE_B="http://127.0.0.1:$PORT_B"

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

api_a() {
    local method="$1" path="$2"
    shift 2
    curl -s -X "$method" "$BASE_A$path" \
        -H 'Content-Type: application/json' "$@" 2>/dev/null || echo '{"error":"curl failed"}'
}

api_b() {
    local method="$1" path="$2"
    shift 2
    curl -s -X "$method" "$BASE_B$path" \
        -H 'Content-Type: application/json' "$@" 2>/dev/null || echo '{"error":"curl failed"}'
}

session_ids_a() { api_a GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions']))" 2>/dev/null; }
session_ids_b() { api_b GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions']))" 2>/dev/null; }
remote_sessions_a() { api_a GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions'] if s['origin']=='remote'))" 2>/dev/null; }
remote_sessions_b() { api_b GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions'] if s['origin']=='remote'))" 2>/dev/null; }
transport_a() { api_a GET /api/status | python3 -c "import sys,json; ts=json.load(sys.stdin).get('transports',[]); print(' '.join(t['name'] for t in ts))" 2>/dev/null; }
transport_b() { api_b GET /api/status | python3 -c "import sys,json; ts=json.load(sys.stdin).get('transports',[]); print(' '.join(t['name'] for t in ts))" 2>/dev/null; }

# ── Setup: tmux server ──────────────────────────────────────────────
log "Starting tmux server"
tmux new-session -d -s test -x 200 -y 50

cp /bin/sleep /tmp/claude

# 4 panes: 2 per daemon
tmux new-window -t test
PANE_A1=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_A1" '/tmp/claude 3600' Enter

tmux new-window -t test
PANE_A2=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_A2" '/tmp/claude 3600' Enter

tmux new-window -t test
PANE_B1=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_B1" '/tmp/claude 3600' Enter

tmux new-window -t test
PANE_B2=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_B2" '/tmp/claude 3600' Enter

sleep 1

log "Panes: A1=$PANE_A1  A2=$PANE_A2  B1=$PANE_B1  B2=$PANE_B2"
tmux list-panes -a -F '#{pane_id} #{pane_current_command}'

# ── Setup: wait for relay ──────────────────────────────────────────
log "Waiting for Nostr relay at $RELAY_URL..."
for i in $(seq 1 30); do
    if curl -sf --max-time 2 "http://127.0.0.1:8080" >/dev/null 2>&1 || \
       python3 -c "import socket; s=socket.socket(); s.settimeout(1); s.connect(('127.0.0.1',8080)); s.close()" 2>/dev/null; then
        break
    fi
    sleep 1
done
log "Relay ready"

# ── Setup: two ouija daemons with nostr transport ─────────────────
log "Starting daemon A (alpha) on port $PORT_A with nostr relay"
RUST_LOG=ouija=debug ouija start --port $PORT_A --name alpha --data /tmp/ouija-A \
    --relay "$RELAY_URL" >/tmp/daemon-a.log 2>&1 &
PID_A=$!

log "Starting daemon B (beta) on port $PORT_B with nostr relay"
RUST_LOG=ouija=debug ouija start --port $PORT_B --name beta --data /tmp/ouija-B \
    --relay "$RELAY_URL" >/tmp/daemon-b.log 2>&1 &
PID_B=$!

# Wait for both HTTP endpoints
log "Waiting for daemons..."
for i in $(seq 1 50); do
    a_ok=$(curl -sf "$BASE_A/api/status" >/dev/null 2>&1 && echo 1 || echo 0)
    b_ok=$(curl -sf "$BASE_B/api/status" >/dev/null 2>&1 && echo 1 || echo 0)
    if [ "$a_ok" = "1" ] && [ "$b_ok" = "1" ]; then break; fi
    sleep 0.2
done
log "Daemon A started (PID $PID_A)"
log "Daemon B started (PID $PID_B)"

# Wait for nostr transport to be ready (poll /api/ticket until valid)
log "Waiting for nostr transport initialization..."
for i in $(seq 1 30); do
    ticket_a=$(api_a GET "/api/ticket" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('ticket',''))" 2>/dev/null || echo "")
    ticket_b=$(api_b GET "/api/ticket" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('ticket',''))" 2>/dev/null || echo "")
    if [ -n "$ticket_a" ] && [ -n "$ticket_b" ]; then break; fi
    sleep 1
done
log "Nostr transport initialized on both daemons"

# ═══════════════════════════════════════════════════════════════════
# TESTS
# ═══════════════════════════════════════════════════════════════════

log "Test 1: Nostr transport is active on both daemons"
assert_contains "daemon A has nostr transport" "$(transport_a)" "nostr"
assert_contains "daemon B has nostr transport" "$(transport_b)" "nostr"

log "Test 2: Nostr tickets are nprofile bech32 strings"
result_a=$(api_a GET "/api/ticket")
ticket_a=$(echo "$result_a" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ticket',''))" 2>/dev/null || echo "")
result_b=$(api_b GET "/api/ticket")
ticket_b=$(echo "$result_b" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ticket',''))" 2>/dev/null || echo "")
assert_contains "A ticket starts with nprofile" "$ticket_a" "nprofile1"
assert_contains "B ticket starts with nprofile" "$ticket_b" "nprofile1"

log "Test 3: B connects to A using nprofile ticket"
result=$(api_b POST /api/connect -d "{\"ticket\":\"$ticket_a\"}")
# Both daemons start with --relay, so auto-discovery may have already connected them.
# Accept either "connected" or "already connected" as success.
if echo "$result" | grep -qF '"status":"connected"' || echo "$result" | grep -qF '"error":"already connected'; then
    pass "connect returns connected or already-connected"
else
    fail "connect returns status" "connected or already-connected" "$result"
fi

# Give relay time to establish subscriptions
sleep 3

# Register sessions AFTER connect
log "  Registering sessions on both daemons..."
api_a POST /api/register -d "{\"id\":\"sess-alpha\",\"pane\":\"$PANE_A1\"}" >/dev/null
api_b POST /api/register -d "{\"id\":\"sess-beta\",\"pane\":\"$PANE_B1\"}" >/dev/null

log "Test 4: Peer discovery — remote sessions appear with daemon prefix"
log "  Waiting for session propagation via nostr DMs..."
for i in $(seq 1 30); do
    ra=$(remote_sessions_a)
    rb=$(remote_sessions_b)
    a_has_beta=false; echo "$ra" | grep -qF "beta/sess-beta" && a_has_beta=true
    b_has_alpha=false; echo "$rb" | grep -qF "alpha/sess-alpha" && b_has_alpha=true
    if $a_has_beta && $b_has_alpha; then break; fi
    sleep 1
done
assert_contains "A has beta/sess-beta as remote" "$(remote_sessions_a)" "beta/sess-beta"
assert_contains "B has alpha/sess-alpha as remote" "$(remote_sessions_b)" "alpha/sess-alpha"

log "Test 5: Message A->B via nostr DM"
result=$(api_a POST /api/send -d '{"from":"sess-alpha","to":"beta/sess-beta","message":"hello via nostr"}')
assert_contains "send via gossip" "$result" "gossip"
log "  Waiting for nostr DM delivery..."
for i in $(seq 1 20); do
    pane_content=$(tmux capture-pane -t "$PANE_B1" -p -S -30)
    if echo "$pane_content" | grep -qF "hello via nostr"; then break; fi
    sleep 0.5
done
assert_contains "message appears in B's pane" "$pane_content" "hello via nostr"

log "Test 6: Message B->A via nostr DM"
result=$(api_b POST /api/send -d '{"from":"sess-beta","to":"alpha/sess-alpha","message":"reply via nostr"}')
assert_contains "send via gossip" "$result" "gossip"
log "  Waiting for nostr DM delivery..."
for i in $(seq 1 20); do
    pane_content=$(tmux capture-pane -t "$PANE_A1" -p -S -30)
    if echo "$pane_content" | grep -qF "reply via nostr"; then break; fi
    sleep 0.5
done
assert_contains "message appears in A's pane" "$pane_content" "reply via nostr"

log "Test 7: Local delivery still works alongside nostr transport"
api_a POST /api/register -d "{\"id\":\"local-a2\",\"pane\":\"$PANE_A2\"}" >/dev/null
result=$(api_a POST /api/send -d '{"from":"sess-alpha","to":"local-a2","message":"local nostr test"}')
assert_contains "local send delivered" "$result" "delivered"
assert_contains "method is tmux" "$result" "tmux"
sleep 1
pane_content=$(tmux capture-pane -t "$PANE_A2" -p)
assert_contains "local message appears in pane" "$pane_content" "local nostr test"
api_a POST /api/remove -d '{"id":"local-a2"}' >/dev/null

log "Test 8: Session removal propagates via nostr"
api_a POST /api/remove -d '{"id":"sess-alpha"}' >/dev/null
log "  Waiting for removal propagation..."
for i in $(seq 1 20); do
    if ! echo "$(remote_sessions_b)" | grep -qF "alpha/sess-alpha"; then break; fi
    sleep 1
done
assert_not_contains "B no longer has alpha/sess-alpha" "$(session_ids_b)" "alpha/sess-alpha"

log "Test 9: Admin dashboard shows nostr transport"
admin_a=$(curl -s "$BASE_A/admin" 2>/dev/null || echo "")
assert_contains "admin A shows nostr" "$admin_a" "nostr"

# Cleanup remaining sessions
api_b POST /api/remove -d '{"id":"sess-beta"}' >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════
# THREE-DAEMON TESTS — daemon C connects to A via nprofile
# This tests the real-world scenario where a new daemon connects to
# an existing peer using only the nprofile ticket.
# ═══════════════════════════════════════════════════════════════════

PORT_C=7882
BASE_C="http://127.0.0.1:$PORT_C"

api_c() {
    local method="$1" path="$2"
    shift 2
    curl -s -X "$method" "$BASE_C$path" \
        -H 'Content-Type: application/json' "$@" 2>/dev/null || echo '{"error":"curl failed"}'
}

transport_c() { api_c GET /api/status | python3 -c "import sys,json; ts=json.load(sys.stdin).get('transports',[]); print(' '.join(t['name'] for t in ts))" 2>/dev/null; }
remote_sessions_c() { api_c GET /api/status | python3 -c "import sys,json; print(' '.join(s['id'] for s in json.load(sys.stdin)['sessions'] if s['origin']=='remote'))" 2>/dev/null; }

# Extra panes for daemon C
tmux new-window -t test
PANE_C1=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_C1" '/tmp/claude 3600' Enter
sleep 1

log "Starting daemon C (gamma) on port $PORT_C with default relay"
RUST_LOG=ouija=debug ouija start --port $PORT_C --name gamma --data /tmp/ouija-C \
    --relay "$RELAY_URL" >/tmp/daemon-c.log 2>&1 &
PID_C=$!

for i in $(seq 1 50); do
    if curl -sf "$BASE_C/api/status" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
log "Daemon C started (PID $PID_C)"

# Wait for nostr transport to register
for i in $(seq 1 30); do
    tc=$(transport_c)
    if echo "$tc" | grep -qF "nostr"; then break; fi
    sleep 0.5
done

log "Test 10: C has nostr transport"
assert_contains "C has nostr transport" "$(transport_c)" "nostr"

# Re-register a session on A for these tests
api_a POST /api/register -d "{\"id\":\"sess-alpha2\",\"pane\":\"$PANE_A1\"}" >/dev/null

log "Test 11: C connects to A using nprofile ticket"
# Get A's nostr ticket
ticket_a=$(api_a GET "/api/ticket" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ticket',''))" 2>/dev/null || echo "")
result=$(api_c POST /api/connect -d "{\"ticket\":\"$ticket_a\"}")
assert_contains "C connects to A via nostr" "$result" '"status":"connected"'

# Register session on C
api_c POST /api/register -d "{\"id\":\"sess-gamma\",\"pane\":\"$PANE_C1\"}" >/dev/null

log "Test 12: Bidirectional session discovery (A <-> C via nostr)"
log "  Waiting for session propagation..."
for i in $(seq 1 30); do
    ra=$(remote_sessions_a)
    rc=$(remote_sessions_c)
    a_has_gamma=false; echo "$ra" | grep -qF "gamma/sess-gamma" && a_has_gamma=true
    c_has_alpha=false; echo "$rc" | grep -qF "alpha/sess-alpha2" && c_has_alpha=true
    if $a_has_gamma && $c_has_alpha; then break; fi
    sleep 1
done
assert_contains "A sees gamma/sess-gamma" "$(remote_sessions_a)" "gamma/sess-gamma"
assert_contains "C sees alpha/sess-alpha2" "$(remote_sessions_c)" "alpha/sess-alpha2"

log "Test 13: Message A->C via nostr (lazy-activated peer)"
result=$(api_a POST /api/send -d '{"from":"sess-alpha2","to":"gamma/sess-gamma","message":"hello lazy gamma"}')
assert_contains "send via gossip" "$result" "gossip"
log "  Waiting for nostr DM delivery..."
for i in $(seq 1 20); do
    pane_content=$(tmux capture-pane -t "$PANE_C1" -p -S -30)
    if echo "$pane_content" | grep -qF "hello lazy gamma"; then break; fi
    sleep 0.5
done
assert_contains "message appears in C's pane" "$pane_content" "hello lazy gamma"

log "Test 14: Message C->A via nostr (lazy-activated peer sends)"
result=$(api_c POST /api/send -d '{"from":"sess-gamma","to":"alpha/sess-alpha2","message":"reply from gamma"}')
assert_contains "send via gossip" "$result" "gossip"
log "  Waiting for nostr DM delivery..."
for i in $(seq 1 20); do
    pane_content=$(tmux capture-pane -t "$PANE_A1" -p -S -30)
    if echo "$pane_content" | grep -qF "reply from gamma"; then break; fi
    sleep 0.5
done
assert_contains "message appears in A's pane" "$pane_content" "reply from gamma"

# Cleanup
api_a POST /api/remove -d '{"id":"sess-alpha2"}' >/dev/null 2>&1 || true
api_c POST /api/remove -d '{"id":"sess-gamma"}' >/dev/null 2>&1 || true

# ═══════════════════════════════════════════════════════════════════
# UNAUTHORIZED SENDER TEST — daemon D connects without secret
# Verifies that peers who don't present the connect secret are rejected.
# ═══════════════════════════════════════════════════════════════════

PORT_D=7883
BASE_D="http://127.0.0.1:$PORT_D"

api_d() {
    local method="$1" path="$2"
    shift 2
    curl -s -X "$method" "$BASE_D$path" \
        -H 'Content-Type: application/json' "$@" 2>/dev/null || echo '{"error":"curl failed"}'
}

tmux new-window -t test
PANE_D1=$(tmux display-message -t test -p '#{pane_id}')
tmux send-keys -t "$PANE_D1" '/tmp/claude 3600' Enter
sleep 1

log "Starting daemon D (delta) on port $PORT_D — unauthorized sender test"
RUST_LOG=ouija=debug ouija start --port $PORT_D --name delta --data /tmp/ouija-D \
    --relay "$RELAY_URL" >/tmp/daemon-d.log 2>&1 &
PID_D=$!

for i in $(seq 1 50); do
    if curl -sf "$BASE_D/api/status" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
log "Daemon D started (PID $PID_D)"

# Wait for nostr transport
for i in $(seq 1 30); do
    td=$(api_d GET /api/status | python3 -c "import sys,json; ts=json.load(sys.stdin).get('transports',[]); print(' '.join(t['name'] for t in ts))" 2>/dev/null)
    if echo "$td" | grep -qF "nostr"; then break; fi
    sleep 0.5
done

# Re-register a session on A for this test
api_a POST /api/register -d "{\"id\":\"sess-alpha3\",\"pane\":\"$PANE_A1\"}" >/dev/null

# Get A's ticket and strip the secret — D only gets the nprofile
ticket_a=$(api_a GET "/api/ticket" | python3 -c "import sys,json; print(json.load(sys.stdin).get('ticket',''))" 2>/dev/null || echo "")
nprofile_only=$(echo "$ticket_a" | cut -d'#' -f1)

log "Test 15: D connects to A without secret (nprofile only)"
result=$(api_d POST /api/connect -d "{\"ticket\":\"$nprofile_only\"}")
# Connect itself will succeed (adds relay + pubkey locally) but A won't authorize D
if echo "$result" | grep -qF '"status":"connected"' || echo "$result" | grep -qF '"error":"already connected'; then
    pass "D connect call succeeds (local setup)"
else
    fail "D connect call" "connected or already-connected" "$result"
fi

# Register session on D and wait briefly
api_d POST /api/register -d "{\"id\":\"sess-delta\",\"pane\":\"$PANE_D1\"}" >/dev/null
sleep 5

log "Test 16: A does NOT see delta's sessions (unauthorized peer)"
remote_a=$(remote_sessions_a)
assert_not_contains "A does not have delta/sess-delta" "$remote_a" "delta/sess-delta"

log "Test 17: D sends message to A — message NOT delivered (unauthorized)"
# Clear A's pane first
tmux send-keys -t "$PANE_A1" '' Enter
sleep 0.5
result=$(api_d POST /api/send -d '{"from":"sess-delta","to":"alpha/sess-alpha3","message":"unauthorized msg"}')
sleep 5
pane_content=$(tmux capture-pane -t "$PANE_A1" -p -S -30)
assert_not_contains "unauthorized message NOT in A's pane" "$pane_content" "unauthorized msg"

log "Test 18: A's log shows rejection of unauthorized sender"
daemon_a_log=$(cat /tmp/daemon-a.log 2>/dev/null || echo "")
assert_contains "A log has rejection" "$daemon_a_log" "rejected message from unauthorized sender"

# Cleanup
api_a POST /api/remove -d '{"id":"sess-alpha3"}' >/dev/null 2>&1 || true
api_d POST /api/remove -d '{"id":"sess-delta"}' >/dev/null 2>&1 || true

# ── Daemon logs ──────────────────────────────────────────────────────
echo ""
log "Daemon A logs:"
cat /tmp/daemon-a.log 2>/dev/null || true
echo ""
log "Daemon B logs:"
cat /tmp/daemon-b.log 2>/dev/null || true
echo ""
log "Daemon C logs:"
cat /tmp/daemon-c.log 2>/dev/null || true
echo ""
log "Daemon D logs:"
cat /tmp/daemon-d.log 2>/dev/null || true

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
kill $PID_A 2>/dev/null || true
kill $PID_B 2>/dev/null || true
kill $PID_C 2>/dev/null || true
kill $PID_D 2>/dev/null || true
exit "$FAIL"
