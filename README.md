# rtk-mcp

MCP server bridge for [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) — token-optimized CLI output for any MCP-compatible client.

## Measured

Same command, raw vs. through rtk-mcp. Sizes are what the model has to read. RTK 0.49.0, Linux.

| Command | Raw | Through rtk-mcp | Reduction |
|---------|----:|----------------:|----------:|
| `cargo test` (this repo, 45 tests) | 2,749 B | 39 B | 98.6% |
| `git diff 09e3d7d 4444d7b` (the 0.5.0 commit) | 140,137 B | 18,387 B | 86.9% |
| `ls -la /usr/bin` | 131,046 B | 1,577 B | 98.8% |

The whole `cargo test` result is one line: `cargo test: 45 passed (1 suite, 2.44s)`.

The filtering is RTK's work; rtk-mcp is the bridge that lets MCP clients such as Claude Desktop use it. RTK summarizes, so it is lossy by design (`ls -la` drops owner and timestamps, `git diff` is condensed). When you need every line, prefix the command with `RTK_DISABLED=1` and you get the raw output.

To reproduce, compare `<command> | wc -c` with `rtk <command> | wc -c`. Numbers vary with the repo, the machine and the RTK version.

Platforms: Linux and macOS. Windows is not supported yet (the server needs a Unix shell). CI runs on Linux.

## What it does

`rtk-mcp` exposes MCP tools that route shell commands through [RTK](https://github.com/rtk-ai/rtk) for **60-90% token reduction** before the output reaches your LLM's context window.

It uses `rtk rewrite` — RTK's canonical hook mechanism — to determine how each command should be filtered. Commands RTK knows about get the full token savings; everything else runs directly. The exit code of `rtk rewrite` is honoured too: a command that matches a `permissions.deny` rule in your Claude settings is refused, never executed.

```
MCP Client (Claude Desktop, Cursor, Windsurf, ...)
  → rtk-mcp (this server)
    → rtk rewrite "cargo test"  →  "rtk cargo test"
    → sh -c "rtk cargo test"    ← filtered output (2,749 B → 39 B)
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

From the original rtk-mcp README: measured over 25 days of its author's daily use with RTK (not re-measured here; the table at the top is):

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

Install [RTK](https://github.com/rtk-ai/rtk) first (any of its official methods):

```bash
brew install rtk                                  # macOS / Linux
curl -fsSL https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh | sh
cargo install --git https://github.com/rtk-ai/rtk # from source (not `cargo install rtk` — different crate)

# Verify
rtk --version   # Should show "rtk X.Y.Z"
rtk gain        # Should work (not "command not found")
```

rtk-mcp is tested against RTK 0.34.3 through 0.49.0 (the full and the `rtk-oss` edition both work — it only needs `rtk rewrite`). Newer RTK adds options to the stats tools (e.g. `view=recalls` needs 0.49+); on an older RTK those return a clear "upgrade RTK" error instead of failing silently.

### Build rtk-mcp

```bash
git clone https://github.com/admiralhr99/rtk-mcp.git
cd rtk-mcp
cargo build --release
```

The binary is at `target/release/rtk-mcp`. Building needs Rust 1.89 or newer (`rustup update stable`).

### Quick install (macOS / Linux)

```bash
./install.sh
```

The installer downloads RTK if it is missing (SHA-256 verified against the release's `checksums.txt`), builds the checkout it is run from, and adds the server to Claude Desktop's config automatically.

## Configure your MCP client

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS) or `~/.config/Claude/claude_desktop_config.json` (Linux):

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
| `get_rtk_savings` | Token savings statistics (`rtk gain`): graph, history, daily/weekly/monthly, failures, recalls, quota, JSON/CSV |
| `get_rtk_discover` | Find missed RTK savings from Claude Code history (`rtk discover`) |
| `get_rtk_session` | Show RTK adoption across recent sessions |

The three stats tools are read-only (`readOnlyHint`), so clients can auto-approve them. `rtk gain --reset` is deliberately not reachable.

### `run_command` parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | Yes | The command to execute (e.g. `git status`, `cargo test`) |
| `cwd` | string | No | Working directory (defaults to server cwd). Must exist; with `RTK_MCP_CWD_JAIL` set it must lie inside that root |

Shell operators are fully supported: `|`, `&&`, `||`, `;`, `>`, `<`, `$VAR`, `*`, `~`.

Commands run under `sh -c` — POSIX sh (dash on Debian/Ubuntu/Mint), not bash. Wrap bash-only syntax (`{1..5}`, `[[ ]]`, `<(...)`, `&>`) in `bash -c '...'`.

Two RTK conventions work through `run_command` as-is:

- `RTK_DISABLED=1 <command>` runs the command raw, with no filtering.
- When RTK elides part of an output it prints a recall hint (`rtk recall <id>`, or a saved-file path in tee mode); run that with `run_command` to get the full text.

### `get_rtk_savings` parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `view` | enum | `summary` (default), `history`, `daily`, `weekly`, `monthly`, `all`, `failures`, `recalls` (RTK 0.49+), `quota` |
| `graph` | bool | ASCII bar chart (`--graph`) |
| `format` | enum | `text` (default), `json`, `csv` |
| `project` / `cwd` | bool / string | Only count the project at `cwd` (`--project`) |
| `tier` | enum | `pro`, `5x`, `20x` — for `view=quota` |

### `get_rtk_discover` parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `all` | bool | Scan all projects (the server's own cwd is rarely your project) |
| `project` | string | Filter by project path (substring) |
| `since` | integer | Only sessions from the last N days (default 30) |
| `limit` | integer | Max commands per section (default 15) |
| `format` | enum | `text` (default), `json` |

Example tool calls from the LLM:

```json
{"name": "run_command", "arguments": {"command": "git log --oneline -5", "cwd": "/my/project"}}
{"name": "run_command", "arguments": {"command": "cargo test | grep FAILED"}}
{"name": "run_command", "arguments": {"command": "ls -la src/"}}
{"name": "run_command", "arguments": {"command": "nmap -sV 10.0.0.1"}}
```

## Security

### Command filtering

When RTK is available, `rtk rewrite` is the gate. Its exit code decides: `1` means RTK has no filter (the command runs as given), `0`/`3` mean rewritten (run the rewrite), and `2` means a `permissions.deny` rule in your Claude settings (`~/.claude/settings.json`, or the project's `.claude/settings.json` found from `cwd`) matches — the command is refused and never executed. `allow`/`ask` rules are not enforced: your MCP client's approval prompt for `run_command` is the gate for those. When RTK is absent and `RTK_MCP_ALLOW_ALL` is not set, the built-in allowlist is enforced.

**Allowlist** (enforced only when RTK is absent and `RTK_MCP_ALLOW_ALL` is unset): `git`, `cargo`, `npm`, `npx`, `pnpm`, `pytest`, `ruff`, `mypy`, `pip`, `uv`, `go`, `golangci-lint`, `docker`, `grep`, `find`, `ls`, `cat`, `head`, `tail`, `wc`, `env`, `echo`, `pwd`, `gh`, `curl`, `wget`, `node`, `tsc`, `next`, `prettier`, `eslint`, `biome`, `playwright`, `prisma`, `vitest`, `dotnet`, `psql`, `make`, `tree`, `bash`, `sh`, `zsh`, `tar`, `unzip`, `zip`, `nmap`, `nuclei`, `httpx`, `aws`, `kubectl`, and more.

### Other protections

- **stdin isolation**: Child processes never inherit the MCP server's stdin fd (prevents EAGAIN transport crashes on macOS)
- **Process-tree kill**: every command leads its own process group; on timeout, or when the client cancels the call, the whole tree is SIGKILLed (a timed-out `nmap`/`ffuf` does not keep running behind `sh`). The output printed before a timeout comes back with the timeout error, so half a scan is not lost. Commands that exit on their own keep any background jobs they deliberately started
- **Background jobs**: a job that was started with its output still attached (`ssh -fN …`, `nohup server &`, `sleep 30 &`) does not hold the call until the timeout. Once the command itself has exited its output gets about 2 seconds to arrive, the call returns with a note, and the job keeps running; whatever it prints later is discarded (read and dropped, so it can neither block on a full pipe nor die of SIGPIPE). Redirect to a file (`cmd > out.log 2>&1 &`) and the call returns at once, without the note
- **Bounded memory**: at most 4× the output cap (1MB by default) is held per stream while a command runs (the rest is drained), and 256KB is returned (`RTK_MCP_MAX_OUTPUT_KB`) — truncation is UTF-8 safe
- **Working-directory jail**: set `RTK_MCP_CWD_JAIL=/some/root` and a `cwd` outside it (after `..` and symlinks are resolved) is refused before anything runs. It guards the `cwd` parameter only — it is not a sandbox: the command itself can still `cd` elsewhere or name absolute paths
- **Quiet children**: `RTK_SUPPRESS_HOOK_WARNING=1` is set for child processes (unless you set it) so RTK's daily "no hook installed" nag never reaches the LLM
- **Length limit**: Commands capped at 4096 characters
- **RTK validation**: Verifies `rtk rewrite` works correctly at startup
- **Exit code propagation**: Failed commands return `isError: true` in MCP response
- **Timeout**: Configurable via `RTK_MCP_TIMEOUT_SECS` (default 300s)

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
                                          │  → 39 B, not 2.7 KB  │
                                          └──────────────────────┘
```

1. MCP client sends a `tools/call` request with a command string
2. `rtk-mcp` calls `rtk rewrite -- "<command>"` (in the command's `cwd`) to get the RTK-optimized form, or a deny verdict
3. Executes the rewritten command (or the original if RTK has no filter) via `sh -c`
4. Returns the output with exit code information

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RTK_MCP_TIMEOUT_SECS` | `300` | Command execution timeout in seconds (a positive number; anything else uses the default) |
| `RTK_MCP_MAX_OUTPUT_KB` | `256` | Largest output returned to the LLM, in KB (a positive number; anything else uses the default) |
| `RTK_MCP_CWD_JAIL` | unset | Directory that every `cwd` must lie inside |
| `RTK_MCP_ALLOW_ALL` | `1` if RTK is installed, else `0` | Bypass the built-in command allowlist |
| `RTK_SUPPRESS_HOOK_WARNING` | `1` for children | Set by the server unless you set it yourself |

The server resolves `rtk` through `PATH`. To pin a specific RTK, put its directory first in `PATH` in your client's `env` block.

## Development

```bash
# Unit tests
cargo test

# Build
cargo build --release

# End-to-end test over real MCP stdio (uses the `rtk` on PATH in a throwaway HOME)
python3 tests/functional.py

# Test MCP protocol manually
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' | ./target/release/rtk-mcp
```

## Credits

- [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) by [rtk-ai](https://github.com/rtk-ai) — all filtering and rewrite logic
- [rtk-mcp](https://github.com/ousamabenyounes/rtk-mcp) by [Ousama Ben Younes](https://github.com/ousamabenyounes) — the original MCP bridge this project started from (initial server, CI and the macOS installer)
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
- Built with [Claude Code](https://claude.ai/code)

## License

MIT, see [LICENSE](LICENSE).
