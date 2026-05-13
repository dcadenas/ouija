#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_LOCAL="$SCRIPT_DIR/docker-compose.yml"
COMPOSE_NOSTR="$SCRIPT_DIR/docker-compose.nostr.yml"
COMPOSE_INSTALL="$SCRIPT_DIR/docker-compose.install.yml"
COMPOSE_OPENCODE="$SCRIPT_DIR/docker-compose.opencode.yml"
COMPOSE_OPTS="up --build --force-recreate --abort-on-container-exit --exit-code-from tests"
DEFAULT_TIMEOUT="${OUIJA_E2E_TIMEOUT:-360s}"

run_compose() {
    local label="$1"
    local project="$2"
    local compose_file="$3"
    local timeout_secs="${4:-$DEFAULT_TIMEOUT}"

    echo "=== Running ${label} e2e tests (timeout: ${timeout_secs}) ==="
    if timeout --kill-after=30s "$timeout_secs" \
        docker compose -p "$project" -f "$compose_file" $COMPOSE_OPTS; then
        return 0
    fi
    local status=$?
    if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
        echo "=== ${label} e2e timed out after ${timeout_secs}; cleaning up compose project ${project} ===" >&2
        docker compose -p "$project" -f "$compose_file" down -v --remove-orphans >/dev/null 2>&1 || true
    fi
    return "$status"
}

run_local() {
    run_compose "local" "ouija-e2e-local" "$COMPOSE_LOCAL" "${OUIJA_E2E_LOCAL_TIMEOUT:-$DEFAULT_TIMEOUT}"
}

run_nostr() {
    run_compose "nostr" "ouija-e2e-nostr" "$COMPOSE_NOSTR" "${OUIJA_E2E_NOSTR_TIMEOUT:-$DEFAULT_TIMEOUT}"
}

run_install() {
    run_compose "install" "ouija-e2e-install" "$COMPOSE_INSTALL" "${OUIJA_E2E_INSTALL_TIMEOUT:-$DEFAULT_TIMEOUT}"
}

run_opencode() {
    run_compose "opencode" "ouija-e2e-opencode" "$COMPOSE_OPENCODE" "${OUIJA_E2E_OPENCODE_TIMEOUT:-$DEFAULT_TIMEOUT}"
}

run_parallel() {
    echo "=== Running local + nostr + opencode e2e tests in parallel ==="
    local local_log=$(mktemp)
    local nostr_log=$(mktemp)
    local opencode_log=$(mktemp)
    local local_ok=0 nostr_ok=0 opencode_ok=0

    run_compose "local" "ouija-e2e-local" "$COMPOSE_LOCAL" "${OUIJA_E2E_LOCAL_TIMEOUT:-$DEFAULT_TIMEOUT}" >"$local_log" 2>&1 &
    local local_pid=$!
    run_compose "nostr" "ouija-e2e-nostr" "$COMPOSE_NOSTR" "${OUIJA_E2E_NOSTR_TIMEOUT:-$DEFAULT_TIMEOUT}" >"$nostr_log" 2>&1 &
    local nostr_pid=$!
    run_compose "opencode" "ouija-e2e-opencode" "$COMPOSE_OPENCODE" "${OUIJA_E2E_OPENCODE_TIMEOUT:-$DEFAULT_TIMEOUT}" >"$opencode_log" 2>&1 &
    local opencode_pid=$!

    wait $local_pid && local_ok=1 || true
    wait $nostr_pid && nostr_ok=1 || true
    wait $opencode_pid && opencode_ok=1 || true

    echo ""
    echo "=== Local test output ==="
    cat "$local_log"
    echo ""
    echo "=== Nostr test output ==="
    cat "$nostr_log"
    echo ""
    echo "=== OpenCode test output ==="
    cat "$opencode_log"
    rm -f "$local_log" "$nostr_log" "$opencode_log"

    if [ "$local_ok" -eq 1 ] && [ "$nostr_ok" -eq 1 ] && [ "$opencode_ok" -eq 1 ]; then
        echo "=== ALL SUITES PASSED ==="
        exit 0
    else
        [ "$local_ok" -eq 0 ] && echo "=== LOCAL TESTS FAILED ==="
        [ "$nostr_ok" -eq 0 ] && echo "=== NOSTR TESTS FAILED ==="
        [ "$opencode_ok" -eq 0 ] && echo "=== OPENCODE TESTS FAILED ==="
        exit 1
    fi
}

case "${1:-all}" in
    local) run_local ;;
    nostr) run_nostr ;;
    install) run_install ;;
    opencode) run_opencode ;;
    all) run_parallel ;;
    seq)
        run_local
        run_nostr
        run_opencode
        ;;
    *)
        echo "Usage: $0 [local|nostr|install|opencode|all|seq]"
        echo "  local    — run local tests only"
        echo "  nostr    — run nostr tests only"
        echo "  install  — run install/preflight tests only"
        echo "  opencode — run opencode integration tests (no API key needed)"
        echo "  all      — run local+nostr+opencode in parallel (default)"
        echo "  seq      — run local+nostr+opencode sequentially"
        exit 1
        ;;
esac
