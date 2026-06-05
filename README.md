# rtk-mcp

MCP server bridge for [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) — token-optimized CLI output for any MCP-compatible client.

## What it does

`rtk-mcp` exposes MCP tools that route shell commands through [RTK](https://github.com/rtk-ai/rtk) for **60-90% token reduction** before the output reaches your LLM's context window.

It uses `rtk rewrite` — RTK's canonical hook mechanism — to determine how each command should be filtered. Commands RTK knows about get the full token savings; everything else runs directly.

```
MCP Client (Claude Desktop, Cursor, Windsurf, ...)
  → rtk-mcp (this server)
    → rtk rewrite "cargo test"  →  "rtk cargo test"
    → sh -c "rtk cargo test"    ← filtered output (97% fewer tokens)
    → returns to LLM

    → rtk rewrite "nmap -sV …"  →  (no RTK filter)
    → sh -c "nmap -sV …"        ← direct execution, full output
    → returns to LLM
```

Without RTK installed, all commands execute directly (no filtering, no token savings).

## Why

[RTK](https://github.com/rtk-ai/rtk) already saves tokens for **Claude Code** and **Gemini CLI** via hooks. But hooks are client-specific — each new AI tool needs its own integration.

MCP is a universal protocol. One server, every client:

| Client | MCP Support |
|--------|-------------|
| Claude Desktop | Yes |
| Cursor | Yes |
| Windsurf | Yes |
| Cline (VS Code) | Yes |
| Continue | Yes |
| Zed | Yes |
| VS Code (native) | Yes |
| GitHub Copilot | Yes |

## Real-world savings

Measured over 25 days of daily usage with RTK:

| Filter | Token reduction |
|--------|----------------|
| `cargo test` | 97.8% |
| `env` | 99.3% |
| `cargo clippy` | 92.5% |
| `find` | 79.2% |
| `ls` | 67-78% |
| `grep` | 64.4% |

Total: **5.3M tokens saved** across 4,876 commands.

## Install

### Prerequisites

Install [RTK](https://github.com/rtk-ai/rtk) first:

```bash
cargo install --git https://github.com/rtk-ai/rtk

# Verify
rtk --version   # Should show "rtk X.Y.Z"
rtk gain        # Should work (not "command not found")
```

### Build rtk-mcp

```bash
git clone https://github.com/admiralhr99/rtk-mcp.git
cd rtk-mcp
cargo build --release
```

The binary is at `target/release/rtk-mcp`.

### Quick install (macOS)

```bash
./install.sh
```

The installer builds the binary and adds it to Claude Desktop's config automatically.

## Configure your MCP client

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "rtk": {
      "command": "/path/to/rtk-mcp",
      "env": {
        "RTK_MCP_ALLOW_ALL": "1"
      }
    }
  }
}
```

### Cursor

Add to `.cursor/mcp.json` in your project:

```json
{
  "mcpServers": {
    "rtk": {
      "command": "/path/to/rtk-mcp"
    }
  }
}
```

### VS Code / Windsurf / Cline

Add to your MCP settings (check each client's documentation for the exact config file path):

```json
{
  "mcpServers": {
    "rtk": {
      "command": "/path/to/rtk-mcp"
    }
  }
}
```

## Tools

| Tool | Description |
|------|-------------|
| `run_command` | Execute any shell command with automatic RTK filtering |
| `get_rtk_savings` | Show token savings statistics (`graph=true` for bar chart) |
| `get_rtk_discover` | Find missed RTK savings from Claude Code history |
| `get_rtk_session` | Show RTK adoption across recent sessions |

### `run_command` parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | Yes | The command to execute (e.g. `git status`, `cargo test`) |
| `cwd` | string | No | Working directory (defaults to server cwd) |

Shell operators are fully supported: `|`, `&&`, `||`, `;`, `>`, `<`, `$VAR`, `*`, `~`.

Example tool calls from the LLM:

```json
{"name": "run_command", "arguments": {"command": "git log --oneline -5", "cwd": "/my/project"}}
{"name": "run_command", "arguments": {"command": "cargo test | grep FAILED"}}
{"name": "run_command", "arguments": {"command": "ls -la src/"}}
{"name": "run_command", "arguments": {"command": "nmap -sV 10.0.0.1"}}
```

## Security

### Command filtering

When RTK is available, `rtk rewrite` is the security gate — commands not known to RTK execute directly without filtering. When RTK is absent and `RTK_MCP_ALLOW_ALL` is not set, the built-in allowlist is enforced.

**Allowlist** (enforced only when RTK is absent and `RTK_MCP_ALLOW_ALL` is unset): `git`, `cargo`, `npm`, `npx`, `pnpm`, `pytest`, `ruff`, `mypy`, `pip`, `uv`, `go`, `golangci-lint`, `docker`, `grep`, `find`, `ls`, `cat`, `head`, `tail`, `wc`, `env`, `echo`, `pwd`, `gh`, `curl`, `wget`, `node`, `tsc`, `next`, `prettier`, `eslint`, `biome`, `playwright`, `prisma`, `vitest`, `dotnet`, `psql`, `make`, `tree`, `nmap`, `nuclei`, `httpx`, `aws`, `kubectl`, and more.

### Other protections

- **stdin isolation**: Child processes never inherit the MCP server's stdin fd (prevents EAGAIN transport crashes on macOS)
- **kill_on_drop**: Timed-out processes are killed immediately to prevent orphaned processes
- **Length limit**: Commands capped at 4096 characters
- **RTK validation**: Verifies `rtk rewrite` works correctly at startup
- **Exit code propagation**: Failed commands return `isError: true` in MCP response
- **Timeout**: Configurable via `RTK_MCP_TIMEOUT_SECS` (default 120s)

## How it works

```
┌──────────────┐     stdio (JSON-RPC)     ┌──────────────────────┐
│  MCP Client  │ ◄──────────────────────► │      rtk-mcp         │
│  (Cursor,    │                          │                      │
│   Claude     │                          │  1. Validate input   │
│   Desktop)   │                          │  2. rtk rewrite cmd  │
│              │                          │  3. sh -c <result>   │
└──────────────┘                          └──────────┬───────────┘
                                                     │
                                          ┌──────────▼───────────┐
                                          │         rtk          │
                                          │   (rewrite + filter) │
                                          │                      │
                                          │ "cargo test"         │
                                          │  → rtk cargo test    │
                                          │  → 97% fewer tokens  │
                                          └──────────────────────┘
```

1. MCP client sends a `tools/call` request with a command string
2. `rtk-mcp` calls `rtk rewrite "<command>"` to get the RTK-optimized form
3. Executes the rewritten command (or the original if RTK has no filter) via `sh -c`
4. Returns the output with exit code information

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RTK_MCP_TIMEOUT_SECS` | `120` | Command execution timeout in seconds |
| `RTK_MCP_ALLOW_ALL` | `1` | Bypass the built-in command allowlist |

## Development

```bash
# Run tests
cargo test

# Build
cargo build --release

# Test MCP protocol manually
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' | ./target/release/rtk-mcp
```

## Credits

- [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) by [rtk-ai](https://github.com/rtk-ai) — all filtering and rewrite logic
- [rmcp](https://github.com/4t145/rmcp) — Rust MCP SDK
- Built with [Claude Code](https://claude.ai/code)

## License

MIT
