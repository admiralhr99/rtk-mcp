#!/bin/bash
# RTK-MCP Installer for macOS and Linux
# Downloads a pre-built RTK (SHA-256 verified) — no Rust needed for RTK itself
set -e

echo "╔══════════════════════════════════════════════╗"
echo "║  RTK-MCP Installer                          ║"
echo "║  Token-optimized CLI for Claude Desktop     ║"
echo "╚══════════════════════════════════════════════╝"
echo ""

INSTALL_DIR="$HOME/.local/bin"
mkdir -p "$INSTALL_DIR"

# Detect platform
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
    Darwin-arm64)              RTK_TARBALL="rtk-aarch64-apple-darwin.tar.gz" ;;
    Darwin-x86_64)             RTK_TARBALL="rtk-x86_64-apple-darwin.tar.gz" ;;
    Linux-x86_64)              RTK_TARBALL="rtk-x86_64-unknown-linux-musl.tar.gz" ;;
    Linux-aarch64|Linux-arm64) RTK_TARBALL="rtk-aarch64-unknown-linux-gnu.tar.gz" ;;
    *)
        echo "ERROR: unsupported platform: $OS $ARCH (macOS and Linux on x86_64/arm64 only)"
        exit 1
        ;;
esac

# 1. Install RTK (pre-built binary from GitHub releases)
if command -v rtk &>/dev/null && rtk --version 2>/dev/null | grep -q "^rtk "; then
    echo "✓ RTK $(rtk --version 2>/dev/null | head -1) already installed"
else
    echo "Downloading RTK..."
    # Pin with RTK_VERSION=vX.Y.Z. Otherwise /releases/latest redirects to /releases/tag/<tag>,
    # which names the newest tag without the (rate-limited) API.
    RTK_VERSION="${RTK_VERSION:-$(curl -fsSIL -o /dev/null -w '%{url_effective}' https://github.com/rtk-ai/rtk/releases/latest | sed 's|.*/||')}"
    if [[ ! "$RTK_VERSION" =~ ^v[0-9] ]]; then
        echo "ERROR: could not determine the latest RTK release (got '${RTK_VERSION}'). Set RTK_VERSION=vX.Y.Z to pin one."
        exit 1
    fi
    RTK_URL="https://github.com/rtk-ai/rtk/releases/download/${RTK_VERSION}"
    TMP_DIR=$(mktemp -d)
    trap 'rm -rf "$TMP_DIR"' EXIT
    curl -fsSL "${RTK_URL}/${RTK_TARBALL}" -o "$TMP_DIR/$RTK_TARBALL"
    curl -fsSL "${RTK_URL}/checksums.txt" -o "$TMP_DIR/checksums.txt"

    # Refuse to install anything that does not match the release's published checksum
    EXPECTED=$(grep "[[:space:]]${RTK_TARBALL}\$" "$TMP_DIR/checksums.txt" | awk '{print $1}')
    if command -v sha256sum &>/dev/null; then
        ACTUAL=$(sha256sum "$TMP_DIR/$RTK_TARBALL" | awk '{print $1}')
    else
        ACTUAL=$(shasum -a 256 "$TMP_DIR/$RTK_TARBALL" | awk '{print $1}')
    fi
    if [[ -z "$EXPECTED" || "$EXPECTED" != "$ACTUAL" ]]; then
        echo "ERROR: checksum verification failed for ${RTK_TARBALL} (expected '${EXPECTED}', got '${ACTUAL}') — refusing to install"
        exit 1
    fi
    # only the executable: RTK 1.x tarballs also hold its licence files
    tar xzf "$TMP_DIR/$RTK_TARBALL" -C "$INSTALL_DIR" rtk
    echo "✓ RTK ${RTK_VERSION} installed to $INSTALL_DIR/rtk (checksum verified)"
fi

# 2. Build rtk-mcp (needs Rust >= 1.89 for rmcp + deps — small project, ~30s build)
MIN_RUST_MINOR=89
if ! command -v cargo &>/dev/null; then
    echo ""
    echo "Rust is needed to build rtk-mcp (one-time, ~30s)."
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --quiet
    source "$HOME/.cargo/env"
else
    RUST_MINOR=$(rustc --version | sed 's/rustc [0-9]*\.\([0-9]*\)\..*/\1/')
    if [[ "$RUST_MINOR" -lt "$MIN_RUST_MINOR" ]]; then
        if ! command -v rustup &>/dev/null; then
            echo "ERROR: Rust 1.${RUST_MINOR} is too old (need >= 1.${MIN_RUST_MINOR}) and rustup is not available to update it."
            echo "Install rustup from https://rustup.rs, then run this again."
            exit 1
        fi
        echo "Rust too old (1.${RUST_MINOR}), updating to latest..."
        rustup update stable
        hash -r
    fi
fi
# Reload PATH to pick up updated cargo/rustc
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
hash -r
echo "✓ Rust $(rustc --version)"

# Run from a checkout: build that. Otherwise keep a clone under ~/.local/share.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -f "$SCRIPT_DIR/Cargo.toml" ]] && grep -q '^name = "rtk-mcp"' "$SCRIPT_DIR/Cargo.toml"; then
    MCP_DIR="$SCRIPT_DIR"
    cd "$MCP_DIR"
else
    MCP_DIR="$HOME/.local/share/rtk-mcp"
    if [[ -d "$MCP_DIR" ]]; then
        echo "Updating rtk-mcp..."
        cd "$MCP_DIR" && git pull --ff-only --quiet
    else
        echo "Cloning rtk-mcp..."
        git clone --quiet https://github.com/admiralhr99/rtk-mcp.git "$MCP_DIR"
        cd "$MCP_DIR"
    fi
fi

echo "Building rtk-mcp..."
cargo build --release --quiet
BINARY="$MCP_DIR/target/release/rtk-mcp"
echo "✓ rtk-mcp built"

# 3. Configure Claude Desktop
if [[ "$OS" == "Darwin" ]]; then
    CONFIG_DIR="$HOME/Library/Application Support/Claude"
else
    CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/Claude"
fi
CONFIG_FILE="$CONFIG_DIR/claude_desktop_config.json"
mkdir -p "$CONFIG_DIR"

if [[ -f "$CONFIG_FILE" ]]; then
    if grep -q "rtk" "$CONFIG_FILE" 2>/dev/null; then
        echo "✓ Claude Desktop already configured"
    else
        python3 - "$CONFIG_FILE" "$BINARY" <<'PYEOF'
import json, sys
config_file, binary = sys.argv[1], sys.argv[2]
with open(config_file) as f:
    config = json.load(f)
config.setdefault('mcpServers', {})
config['mcpServers']['rtk'] = {'command': binary}
with open(config_file, 'w') as f:
    json.dump(config, f, indent=2)
print('✓ Claude Desktop config updated')
PYEOF
    fi
else
    cat > "$CONFIG_FILE" << JSONEOF
{
  "mcpServers": {
    "rtk": {
      "command": "$BINARY"
    }
  }
}
JSONEOF
    echo "✓ Claude Desktop config created"
fi

# 4. Add to PATH if needed
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    case "${SHELL##*/}" in
        zsh) SHELL_RC="$HOME/.zshrc" ;;
        *)   SHELL_RC="$HOME/.bashrc" ;;
    esac
    echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$SHELL_RC"
    echo "✓ Added $INSTALL_DIR to PATH in $SHELL_RC (restart terminal)"
fi

echo ""
echo "╔══════════════════════════════════════════════╗"
echo "║  Done! Restart Claude Desktop               ║"
echo "║  (macOS: Cmd+Q, then reopen)                ║"
echo "║  Then ask Claude: 'run ls -la'              ║"
echo "╚══════════════════════════════════════════════╝"
