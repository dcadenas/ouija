#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
OUIJA_PORT=7880

# Colors
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

step=0
step() {
    step=$((step + 1))
    echo -e "\n${BOLD}[$step]${NC} $1"
}
ok()   { echo -e "    ${GREEN}OK${NC} $1"; }
skip() { echo -e "    ${YELLOW}SKIP${NC} $1"; }
fail() { echo -e "    ${RED}FAIL${NC} $1"; exit 1; }

# --- Detect OS and package manager ---
step "Detecting OS..."
install_pkg() {
    local pkg=$1
    if command -v pacman &>/dev/null; then
        sudo pacman -S --noconfirm "$pkg"
    elif command -v apt-get &>/dev/null; then
        sudo apt-get update -qq && sudo apt-get install -y "$pkg"
    elif command -v brew &>/dev/null; then
        brew install "$pkg"
    else
        fail "No supported package manager found (need pacman, apt-get, or brew)"
    fi
}

if [[ "$(uname)" == "Darwin" ]]; then
    ok "macOS"
    if ! command -v brew &>/dev/null; then
        echo "    Installing Homebrew..."
        /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
    fi
elif [[ -f /etc/os-release ]]; then
    . /etc/os-release
    ok "Linux ($PRETTY_NAME)"
else
    fail "Unsupported OS"
fi

# --- Install tmux ---
step "Checking tmux..."
if command -v tmux &>/dev/null; then
    ok "tmux $(tmux -V | awk '{print $2}')"
else
    echo "    Installing tmux..."
    install_pkg tmux
    ok "installed"
fi

# --- Check Claude Code ---
step "Checking Claude Code CLI..."
if command -v claude &>/dev/null; then
    ok "claude found"
else
    echo -e "    ${YELLOW}Claude Code CLI not found.${NC}"
    echo "    Install it with: npm install -g @anthropic-ai/claude-code"
    echo "    Then re-run this script."
    exit 1
fi

# --- Install ouija ---
step "Installing ouija..."

INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

# Detect target triple
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
    Linux)  OS_PART="unknown-linux-gnu" ;;
    Darwin) OS_PART="apple-darwin" ;;
    *)      fail "unsupported OS: $OS" ;;
esac
case "$ARCH" in
    x86_64)          ARCH_PART="x86_64" ;;
    aarch64|arm64)   ARCH_PART="aarch64" ;;
    *)               fail "unsupported architecture: $ARCH" ;;
esac
TARGET="${ARCH_PART}-${OS_PART}"

# Try downloading precompiled binary from GitHub Releases
INSTALLED=false
RELEASE_JSON=$(curl -sf https://api.github.com/repos/dcadenas/ouija/releases/latest 2>/dev/null || true)
if [[ -n "$RELEASE_JSON" ]]; then
    OUIJA_VERSION=$(echo "$RELEASE_JSON" | python3 -c "import sys,json; v=json.load(sys.stdin)['tag_name']; print(v.lstrip('v'))" 2>/dev/null || true)
    if [[ -n "$OUIJA_VERSION" ]]; then
        echo "    latest release: $OUIJA_VERSION (target: $TARGET)"
        TARBALL_URL="https://github.com/dcadenas/ouija/releases/download/v${OUIJA_VERSION}/ouija-${OUIJA_VERSION}-${TARGET}.tar.gz"
        TMPDIR=$(mktemp -d)
        if curl -fL "$TARBALL_URL" | tar xz -C "$TMPDIR" 2>/dev/null; then
            cp "$TMPDIR/ouija-${OUIJA_VERSION}-${TARGET}/ouija" "$INSTALL_DIR/ouija"
            chmod +x "$INSTALL_DIR/ouija"
            rm -rf "$TMPDIR"
            INSTALLED=true
            ok "ouija $OUIJA_VERSION installed to $INSTALL_DIR/ouija"
        else
            rm -rf "$TMPDIR"
            echo -e "    ${YELLOW}no precompiled binary for $TARGET, trying cargo fallback...${NC}"
        fi
    fi
fi

# Fallback: build from source via cargo
if [[ "$INSTALLED" != "true" ]]; then
    if ! command -v cargo &>/dev/null; then
        echo "    Installing Rust via rustup..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
    fi
    OUIJA_VERSION=$(curl -sf https://crates.io/api/v1/crates/ouija | python3 -c "import sys,json; print(json.load(sys.stdin)['crate']['max_version'])" 2>/dev/null)
    if [[ -z "$OUIJA_VERSION" ]]; then
        fail "could not fetch latest ouija version"
    fi
    echo "    building from source (v$OUIJA_VERSION)..."
    cargo install ouija --version "$OUIJA_VERSION" 2>&1 | tail -1
    ok "ouija $OUIJA_VERSION installed via cargo"
fi

# Ensure ~/.local/bin is on PATH
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo -e "    ${YELLOW}NOTE:${NC} add $INSTALL_DIR to your PATH"
        echo "    e.g. export PATH=\"$INSTALL_DIR:\$PATH\" in your shell profile"
        ;;
esac

# --- Start daemon (skip restart if already healthy) ---
step "Checking daemon..."
if curl -sf "http://localhost:${OUIJA_PORT}/api/status" >/dev/null 2>&1; then
    skip "daemon already running and healthy on port $OUIJA_PORT"
else
    echo "    Starting daemon..."
    if pkill -f 'ouija start' 2>/dev/null; then
        echo "    Stopped old process"
        sleep 1
    fi
    if tmux has-session -t ouija-daemon 2>/dev/null; then
        tmux kill-session -t ouija-daemon
        sleep 1
    fi
    tmux new-session -d -s ouija-daemon "ouija start"
    ok "running in tmux session 'ouija-daemon'"

    echo "    Waiting for daemon..."
    for i in $(seq 1 20); do
        if curl -sf "http://localhost:${OUIJA_PORT}/api/status" >/dev/null 2>&1; then
            ok "daemon responding on port $OUIJA_PORT"
            break
        fi
        if [[ $i -eq 20 ]]; then
            fail "daemon did not start within 10s (check: tmux attach -t ouija-daemon)"
        fi
        sleep 0.5
    done
fi

# --- Register MCP ---
step "Registering MCP with Claude Code..."
MCP_FILE="$HOME/.claude/.mcp.json"
if [[ -f "$MCP_FILE" ]] && grep -q '"ouija"' "$MCP_FILE"; then
    skip "ouija MCP already registered"
else
    if claude mcp add --scope user --transport http ouija "http://localhost:${OUIJA_PORT}/mcp" 2>&1; then
        ok "registered ouija MCP"
    else
        skip "ouija MCP already registered"
    fi
fi

# --- Install peer trust skill ---
step "Installing ouija peer trust skill..."
SKILL_DIR="$HOME/.claude/skills/ouija-peer-trust"
mkdir -p "$SKILL_DIR"
cp "$SCRIPT_DIR/skill/SKILL.md" "$SKILL_DIR/SKILL.md"
ok "installed to $SKILL_DIR"

# --- Done ---
echo ""
echo -e "${GREEN}${BOLD}Setup complete!${NC}"
echo ""
echo -e "${BOLD}How to use:${NC}"
echo ""
echo -e "  ${BLUE}1.${NC} Open a new tmux window and start Claude Code:"
echo -e "     ${BOLD}tmux new-window && claude${NC}"
echo ""
echo -e "  ${BLUE}2.${NC} Ask Claude to register itself:"
echo -e "     ${BOLD}\"Register me as web\"${NC}  (or api, infra, whatever this session does)"
echo ""
echo -e "  ${BLUE}3.${NC} Ask Claude to message another session:"
echo -e "     ${BOLD}\"Tell api to check the auth logs\"${NC}"
echo ""
echo -e "  ${BLUE}4.${NC} Check status anytime:"
echo -e "     ${BOLD}ouija status${NC}"
echo ""
echo -e "  ${BLUE}5.${NC} Dashboard:"
echo -e "     ${BOLD}http://localhost:${OUIJA_PORT}/admin${NC}"
echo ""
echo -e "  ${BLUE}Tip:${NC} The daemon runs in tmux session ${BOLD}ouija-daemon${NC}."
echo -e "       View logs: ${BOLD}tmux attach -t ouija-daemon${NC}"
echo ""
echo -e "  ${YELLOW}Security:${NC} To connect to another machine, exchange tickets"
echo -e "  via CLI (${BOLD}ouija ticket${NC} / ${BOLD}ouija connect <ticket>${NC}) or the dashboard."
echo -e "  Tickets are secrets — never share them through Claude sessions."
echo -e "  Only pair with machines you trust."
