#!/bin/bash
# Shared helpers for ouija e2e tests. Source this from test scripts.

# ── Color constants ─────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

PASS=0
FAIL=0

# ── Logging and assertions ──────────────────────────────────────────
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

# ── wait_for — poll until a command succeeds or timeout ─────────────
# Usage: wait_for TIMEOUT_SECS COMMAND [ARGS...]
# Polls every 0.5s, returns 0 on success, 1 on timeout.
wait_for() {
    local timeout="$1"; shift
    local end=$(( $(date +%s) + timeout ))
    while [ "$(date +%s)" -lt "$end" ]; do
        if "$@" 2>/dev/null; then return 0; fi
        sleep 0.5
    done
    return 1
}

# ── API helper ──────────────────────────────────────────────────────
# Usage: api BASE_URL METHOD PATH [extra curl args...]
api() {
    local base="$1" method="$2" path="$3"
    shift 3
    local args=("$@")
    if [ "$method" = "POST" ] && [ "$path" = "/api/sessions/start" ]; then
        local i
        for i in "${!args[@]}"; do
            if [ "${args[$i]}" = "-d" ] && [ $((i + 1)) -lt "${#args[@]}" ]; then
                args[$((i + 1))]=$(echo "${args[$((i + 1))]}" | jq -c '
                    if type == "object"
                       and (.parent_session == null)
                       and (.no_parent_session == null)
                       and (.idle_policy == null)
                    then . + {"no_parent_session": true, "idle_policy": "keep-open"}
                    else .
                    end
                ')
                break
            fi
        done
    fi
    curl -sf -X "$method" "${base}${path}" \
        -H 'Content-Type: application/json' "${args[@]}" 2>/dev/null || echo '{"error":"curl failed"}'
}

# ── Session query helpers (all take a base URL) ─────────────────────
session_ids() {
    api "$1" GET /api/status | jq -r '[.sessions[].id] | join(" ")'
}

session_count() {
    api "$1" GET /api/status | jq -r '.sessions | length'
}

session_field() {
    local base="$1" sid="$2" field="$3"
    api "$base" GET /api/status | jq -r --arg id "$sid" --arg f "$field" \
        '.sessions[] | select(.id == $id) | .[$f] // ""'
}

persisted_session_incarnation() {
    local sid="$1"
    jq -er --arg id "$sid" \
        '.sessions[] | select(.id == $id) | .metadata.session_incarnation' \
        /tmp/ouija-test/sessions.json
}

persisted_session_launch_completed() {
    local sid="$1"
    local incarnation pane pane_pid physical_session physical_incarnation
    jq -e --arg id "$sid" '
        (.sessions[] | select(.id == $id)) as $session
        | ($session.pane | type == "string")
          and (.lifecycle_leases[$id] == null)
    ' /tmp/ouija-test/sessions.json >/dev/null
    incarnation=$(persisted_session_incarnation "$sid")
    pane=$(jq -er --arg id "$sid" '.sessions[] | select(.id == $id) | .pane' /tmp/ouija-test/sessions.json)
    pane_pid=$(tmux display-message -t "$pane" -p '#{pane_pid}') || return 1
    [ -n "$pane_pid" ] && [ -r "/proc/$pane_pid/environ" ] || return 1
    physical_session=$(tr '\0' '\n' <"/proc/$pane_pid/environ" | sed -n 's/^OUIJA_SESSION_ID=//p')
    physical_incarnation=$(tr '\0' '\n' <"/proc/$pane_pid/environ" | sed -n 's/^OUIJA_SESSION_INCARNATION=//p')
    [ "$physical_session" = "$sid" ] && [ "$physical_incarnation" = "$incarnation" ]
}

persisted_session_restart_completed() {
    local sid="$1" previous_incarnation="$2"
    local incarnation
    incarnation=$(persisted_session_incarnation "$sid")
    [ "$incarnation" != "$previous_incarnation" ] &&
        persisted_session_launch_completed "$sid"
}

remote_session_ids() {
    api "$1" GET /api/status | jq -r '[.sessions[] | select(.origin == "remote") | .id] | join(" ")'
}

transport_names() {
    api "$1" GET /api/status | jq -r '[.transports[].name] | join(" ")'
}

# ── Tmux helpers ────────────────────────────────────────────────────
# Creates a fake "claude" binary from /bin/sleep in the given dir (or a temp dir).
# Prints the directory path.
create_fake_claude() {
    local fake_bin="${1:-$(mktemp -d)}"
    cp /bin/sleep "$fake_bin/claude"
    chmod +x "$fake_bin/claude"
    echo "$fake_bin"
}

# Creates a fake "codex" binary from /bin/sleep in the given dir (or a temp dir).
# Prints the directory path.
create_fake_codex() {
    local fake_bin="${1:-$(mktemp -d)}"
    cp /bin/sleep "$fake_bin/codex"
    chmod +x "$fake_bin/codex"
    echo "$fake_bin"
}

# Creates a new tmux window in the "test" session, rooted at cwd, and runs the
# named fake assistant binary. Prints the pane ID.
create_assistant_pane() {
    local fake_bin="$1" assistant="$2" cwd="$3"
    tmux new-window -t test -c "$cwd"
    local pane
    pane=$(tmux display-message -t test -p '#{pane_id}')
    tmux send-keys -t "$pane" "$fake_bin/$assistant 3600" Enter
    echo "$pane"
}

# Exercises the durable Local identity path against one isolated daemon.
run_identity_continuity_scenario() {
    local base="$1" port="$2" fake_bin="$3"
    local identity_root="/tmp/ouija-test/identity-continuity"
    local claim_project="$identity_root/claim-project"
    local claim_id="continuity-claim"
    local continuity_id="continuity-worker"
    local continuity_project="$identity_root/$continuity_id"
    local claim_backend_id="thread-continuity-claim"
    local continuity_backend_id="claude-continuity-thread"
    local continuity_replacement_pane=""
    local claim_pane continuity_pane

    log "Test 36: Local identity continuity survives pane replacement"
    mkdir -p "$claim_project" "$continuity_project"
    create_fake_codex "$fake_bin" >/dev/null
    claim_pane=$(create_assistant_pane "$fake_bin" codex "$claim_project")
    continuity_pane=$(create_assistant_pane "$fake_bin" claude "$continuity_project")

    IDENTITY_E2E_BASE="$base"
    IDENTITY_E2E_CLAIM_ID="$claim_id"
    IDENTITY_E2E_CONTINUITY_ID="$continuity_id"
    IDENTITY_E2E_CLAIM_PANE="$claim_pane"
    IDENTITY_E2E_CONTINUITY_PANE="$continuity_pane"
    IDENTITY_E2E_REPLACEMENT_PANE=""
    identity_continuity_cleanup() {
        api "$IDENTITY_E2E_BASE" POST /api/settings \
            -d '{"auto_register":false}' >/dev/null 2>&1 || true
        api "$IDENTITY_E2E_BASE" POST /api/remove \
            -d "{\"id\":\"$IDENTITY_E2E_CLAIM_ID\"}" >/dev/null 2>&1 || true
        api "$IDENTITY_E2E_BASE" POST /api/remove \
            -d "{\"id\":\"$IDENTITY_E2E_CONTINUITY_ID\"}" >/dev/null 2>&1 || true
        tmux kill-pane -t "$IDENTITY_E2E_CLAIM_PANE" 2>/dev/null || true
        tmux kill-pane -t "$IDENTITY_E2E_CONTINUITY_PANE" 2>/dev/null || true
        if [ -n "$IDENTITY_E2E_REPLACEMENT_PANE" ]; then
            tmux kill-pane -t "$IDENTITY_E2E_REPLACEMENT_PANE" 2>/dev/null || true
        fi
    }
    # The cleanup is armed before the first mutation of daemon state.
    trap identity_continuity_cleanup EXIT

    # Trusted SessionStart records a transient Local attestation even while
    # automatic registration is disabled. The explicit claim consumes only
    # that exact pane/backend/project evidence.
    local attestation claim_result claim_rc claim_incarnation claim_retry
    attestation=$(api "$base" POST /api/hooks/session-start \
        -d "{\"pane\":\"$claim_pane\",\"cwd\":\"$claim_project\",\"backend_session_id\":\"$claim_backend_id\",\"backend_identity\":{\"backend\":\"codex-cli\",\"session_id\":\"$claim_backend_id\"},\"adapter\":\"codex-cli\"}")
    assert_contains "36a: unregistered Codex remains subject to auto-register policy" \
        "$attestation" "auto_register disabled"
    set +e
    claim_result=$(env -u OUIJA_SESSION_ID TMUX_PANE="$claim_pane" \
        CODEX_THREAD_ID="$claim_backend_id" OUIJA_PORT=$port \
        ouija claim "$claim_id" 2>&1)
    claim_rc=$?
    set -e
    assert_eq "36a: verified Local caller atomically claims requested free id" "$claim_rc" "0"
    assert_contains "36a: first claim reports claimed" "$claim_result" '"outcome":"claimed"'
    claim_incarnation=$(echo "$claim_result" | jq -r '.session_incarnation')

    claim_retry=$(env -u OUIJA_SESSION_ID TMUX_PANE="$claim_pane" \
        CODEX_THREAD_ID="$claim_backend_id" OUIJA_PORT=$port \
        ouija claim "$claim_id")
    assert_contains "36b: exact same-owner retry is current" "$claim_retry" '"outcome":"current"'
    assert_eq "36b: retry preserves incarnation" \
        "$(echo "$claim_retry" | jq -r '.session_incarnation')" "$claim_incarnation"

    # Register a complete Claude owner through the real SessionStart boundary,
    # then simulate its trusted clean-exit hook after the pane has disappeared.
    local register_result prior_incarnation session_end
    api "$base" POST /api/settings -d '{"auto_register":true}' >/dev/null
    register_result=$(api "$base" POST /api/hooks/session-start \
        -d "{\"pane\":\"$continuity_pane\",\"cwd\":\"$continuity_project\",\"backend_session_id\":\"$continuity_backend_id\",\"backend_identity\":{\"backend\":\"claude-code\",\"session_id\":\"$continuity_backend_id\"},\"adapter\":\"claude-code\"}")
    api "$base" POST /api/settings -d '{"auto_register":false}' >/dev/null
    assert_contains "36c: complete Claude fixture registered" \
        "$register_result" "\"registered\":\"$continuity_id\""
    prior_incarnation=$(session_field "$base" "$continuity_id" "session_incarnation")
    tmux kill-pane -t "$continuity_pane"
    session_end=$(api "$base" POST /api/hooks/session-end \
        -d "{\"pane\":\"$continuity_pane\",\"backend_session_id\":\"$continuity_backend_id\",\"session_incarnation\":\"$prior_incarnation\"}")
    assert_contains "36c: trusted SessionEnd parks complete identity" \
        "$session_end" "\"dormant\":\"$continuity_id\""

    local dormant_list dormant_show
    dormant_list=$(env OUIJA_PORT=$port ouija dormant list)
    assert_contains "36d: dormant list contains parked public id" "$dormant_list" "$continuity_id"
    assert_not_contains "36d: dormant list redacts backend-native id" \
        "$dormant_list" "$continuity_backend_id"
    dormant_show=$(env OUIJA_PORT=$port ouija dormant show "$continuity_id")
    assert_contains "36d: dormant show is explicitly non-routable" "$dormant_show" '"routable":false'
    assert_contains "36d: dormant show retains canonical project" \
        "$dormant_show" "$continuity_project"
    assert_not_contains "36d: dormant show redacts backend-native id" \
        "$dormant_show" "$continuity_backend_id"

    # The same complete backend identity in a replacement pane recovers the
    # prior public ID with a daemon-issued successor incarnation.
    local recovery recovered_incarnation
    continuity_replacement_pane=$(create_assistant_pane "$fake_bin" claude "$continuity_project")
    IDENTITY_E2E_REPLACEMENT_PANE="$continuity_replacement_pane"
    recovery=$(api "$base" POST /api/hooks/session-start \
        -d "{\"pane\":\"$continuity_replacement_pane\",\"cwd\":\"$continuity_project\",\"backend_session_id\":\"$continuity_backend_id\",\"backend_identity\":{\"backend\":\"claude-code\",\"session_id\":\"$continuity_backend_id\"},\"adapter\":\"claude-code\"}")
    assert_contains "36e: replacement pane recovers prior public id" \
        "$recovery" "\"registered\":\"$continuity_id\""
    recovered_incarnation=$(echo "$recovery" | jq -r '.session_incarnation')
    if [ "$recovered_incarnation" -gt "$prior_incarnation" ]; then
        pass "36e: recovery receives a newer daemon incarnation"
    else
        fail "36e: recovery receives a newer daemon incarnation" \
            "greater than $prior_incarnation" "$recovered_incarnation"
    fi

    # A rename into an occupied destination fails closed and preserves rows.
    local rename_result rename_rc identity_ids
    set +e
    rename_result=$(env -u OUIJA_SESSION_ID TMUX_PANE="$claim_pane" \
        CODEX_THREAD_ID="$claim_backend_id" OUIJA_PORT=$port \
        ouija rename "$continuity_id" --from "$claim_id" 2>&1)
    rename_rc=$?
    set -e
    assert_eq "36f: occupied rename exits non-zero" "$rename_rc" "1"
    assert_contains "36f: occupied rename explains conflict" "$rename_result" "already exists"
    identity_ids=$(session_ids "$base")
    assert_contains "36f: occupied rename preserves source" "$identity_ids" "$claim_id"
    assert_contains "36f: occupied rename preserves destination" "$identity_ids" "$continuity_id"

    # Park the recovered row once more, then explicitly forget it. Removing
    # identity metadata must never remove the project/worktree directory.
    local unregister_result
    tmux kill-pane -t "$continuity_replacement_pane"
    session_end=$(api "$base" POST /api/hooks/session-end \
        -d "{\"pane\":\"$continuity_replacement_pane\",\"backend_session_id\":\"$continuity_backend_id\",\"session_incarnation\":\"$recovered_incarnation\"}")
    assert_contains "36g: recovered owner can be parked exactly once" \
        "$session_end" "\"dormant\":\"$continuity_id\""
    unregister_result=$(env OUIJA_PORT=$port ouija unregister "$continuity_id")
    assert_contains "36g: exact dormant unregister succeeds" \
        "$unregister_result" "\"forgotten_dormant\":\"$continuity_id\""
    assert_contains "36g: forget response reports preservation" \
        "$unregister_result" '"worktree_preserved":true'
    assert_eq "36g: dormant unregister preserves worktree" \
        "$(test -d "$continuity_project" && echo present)" "present"

    identity_continuity_cleanup
    trap - EXIT
}

# Creates a new tmux window in the "test" session running the fake claude.
# Prints the pane ID.
create_claude_pane() {
    local fake_bin="$1"
    tmux new-window -t test
    local pane
    pane=$(tmux display-message -t test -p '#{pane_id}')
    tmux send-keys -t "$pane" "$fake_bin/claude 3600" Enter
    echo "$pane"
}

# ── MCP JSON-RPC helpers ────────────────────────────────────────────
# The MCP streamable HTTP transport returns SSE (text/event-stream).
# We extract JSON from "data: {..." lines and session ID from headers.
MCP_ID=0
MCP_SESSION=""

mcp_init() {
    local base="$1"
    MCP_ID=$((MCP_ID + 1))
    # Step 1: Send initialize request (SSE keeps connection open, timeout kills it)
    timeout 5 curl -s -D /tmp/mcp-headers -X POST "$base/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"initialize\",\"params\":{\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1.0\"},\"protocolVersion\":\"2025-03-26\"},\"id\":$MCP_ID}" \
        >/tmp/mcp-body 2>/dev/null || true
    MCP_SESSION=$(sed -n 's/^mcp-session-id: *//Ip' /tmp/mcp-headers | tr -d '\r\n')

    # Step 2: Send notifications/initialized (required by MCP before tool calls)
    timeout 2 curl -s -X POST "$base/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $MCP_SESSION" \
        -d '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
        >/dev/null 2>&1 || true

    # Extract JSON from SSE data: lines
    { grep '^data: {' /tmp/mcp-body || true; } | sed 's/^data: //'
}

mcp_call_tool() {
    local base="$1" tool="$2" args="$3"
    MCP_ID=$((MCP_ID + 1))
    timeout 5 curl -s -X POST "$base/mcp" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -H "Mcp-Session-Id: $MCP_SESSION" \
        -d "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"$tool\",\"arguments\":$args},\"id\":$MCP_ID}" \
        >/tmp/mcp-tool-body 2>/dev/null || true
    { grep '^data: {' /tmp/mcp-tool-body || true; } | sed 's/^data: //'
}

# ── Daemon start helper ────────────────────────────────────────────
# Usage: start_daemon PORT NAME DATA_DIR [extra ouija args...]
# Prints the daemon PID. Waits up to 10s for HTTP readiness.
start_daemon() {
    local port="$1" name="$2" data_dir="$3"; shift 3
    mkdir -p "$data_dir"
    # Write default settings only if caller hasn't pre-created one
    if [ ! -f "${data_dir}/settings.json" ]; then
        echo '{"auto_register":false}' > "${data_dir}/settings.json"
    fi
    RUST_LOG=ouija=debug ouija start-server --port "$port" --name "$name" --data "$data_dir" "$@" \
        >"${data_dir}/daemon.log" 2>&1 &
    local pid=$!
    wait_for 10 curl -sf "http://127.0.0.1:${port}/api/status" -o /dev/null
    echo "$pid"
}

# ── Find script helper (used by hook tests) ────────────────────────
find_script() {
    local name="$1"
    local p
    for p in "$(pwd)/scripts/${name}" "/app/scripts/${name}"; do
        [ -f "$p" ] && echo "$p" && return
    done
}

# ── Export helpers for use in bash -c subshells (e.g. wait_for) ────
export -f api session_ids session_count session_field persisted_session_incarnation persisted_session_launch_completed persisted_session_restart_completed remote_session_ids transport_names

# ── Results ─────────────────────────────────────────────────────────
print_results() {
    echo ""
    echo "════════════════════════════════════════════"
    echo -e "Results: ${GREEN}$PASS passed${NC}, ${RED}$FAIL failed${NC}"
    if [ "$FAIL" -eq 0 ]; then
        echo -e "${GREEN}ALL TESTS PASSED${NC}"
    else
        echo -e "${RED}SOME TESTS FAILED${NC}"
    fi
    echo "════════════════════════════════════════════"
}
