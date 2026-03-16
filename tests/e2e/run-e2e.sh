#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMPOSE_LOCAL="$SCRIPT_DIR/docker-compose.yml"
COMPOSE_NOSTR="$SCRIPT_DIR/docker-compose.nostr.yml"
COMPOSE_INSTALL="$SCRIPT_DIR/docker-compose.install.yml"
COMPOSE_OPTS="up --build --abort-on-container-exit --exit-code-from tests"

run_local() {
    echo "=== Running local e2e tests ==="
    docker compose -f "$COMPOSE_LOCAL" $COMPOSE_OPTS
}

run_nostr() {
    echo "=== Running nostr e2e tests ==="
    docker compose -f "$COMPOSE_NOSTR" $COMPOSE_OPTS
}

run_install() {
    echo "=== Running install e2e tests ==="
    docker compose -f "$COMPOSE_INSTALL" $COMPOSE_OPTS
}

run_parallel() {
    echo "=== Running local + nostr e2e tests in parallel ==="
    local local_log=$(mktemp)
    local nostr_log=$(mktemp)
    local local_ok=0 nostr_ok=0

    docker compose -f "$COMPOSE_LOCAL" $COMPOSE_OPTS >"$local_log" 2>&1 &
    local local_pid=$!
    docker compose -f "$COMPOSE_NOSTR" $COMPOSE_OPTS >"$nostr_log" 2>&1 &
    local nostr_pid=$!

    wait $local_pid && local_ok=1 || true
    wait $nostr_pid && nostr_ok=1 || true

    echo ""
    echo "=== Local test output ==="
    cat "$local_log"
    echo ""
    echo "=== Nostr test output ==="
    cat "$nostr_log"
    rm -f "$local_log" "$nostr_log"

    if [ "$local_ok" -eq 1 ] && [ "$nostr_ok" -eq 1 ]; then
        echo "=== ALL SUITES PASSED ==="
        exit 0
    else
        [ "$local_ok" -eq 0 ] && echo "=== LOCAL TESTS FAILED ==="
        [ "$nostr_ok" -eq 0 ] && echo "=== NOSTR TESTS FAILED ==="
        exit 1
    fi
}

case "${1:-all}" in
    local) run_local ;;
    nostr) run_nostr ;;
    install) run_install ;;
    all) run_parallel ;;
    seq)
        run_local
        run_nostr
        ;;
    *)
        echo "Usage: $0 [local|nostr|install|all|seq]"
        echo "  local    — run local tests only"
        echo "  nostr    — run nostr tests only"
        echo "  install  — run install/preflight tests only"
        echo "  all      — run local+nostr in parallel (default)"
        echo "  seq      — run local+nostr sequentially"
        exit 1
        ;;
esac
