# rtk-mcp

MCP server bridge for [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) — token-optimized CLI output for any MCP-compatible client. Runs on Linux, macOS and Windows.

## Measured

Same command, raw vs. through rtk-mcp. Sizes are what the model has to read. RTK 1.1.0, Linux.

| Command | Raw | Through rtk-mcp | Reduction |
|---------|----:|----------------:|----------:|
| `cargo test` (this repo, 76 tests) | 4,849 B | 39 B | 99.2% |
| `git diff 09e3d7d 4444d7b` (the 0.5.0 commit) | 140,137 B | 18,387 B | 86.9% |
| `ls -la /usr/bin` | 131,046 B | 1,581 B | 98.8% |

The whole `cargo test` result is one line: `cargo test: 76 passed (1 suite, 4.42s)`.

The filtering is RTK's work; rtk-mcp is the bridge that lets MCP clients such as Claude Desktop use it. RTK summarizes, so it is lossy by design (`ls -la` drops owner and timestamps, `git diff` is condensed). When you need every line, prefix the command with `RTK_DISABLED=1` and you get the raw output.

To reproduce, compare `<command> | wc -c` with `rtk <command> | wc -c`. Numbers vary with the repo, the machine and the RTK version.

Platforms: Linux, macOS and Windows, with prebuilt binaries for each. What is tested where is under [Platforms](#platforms).

## What it does

`rtk-mcp` exposes MCP tools that route shell commands through [RTK](https://github.com/rtk-ai/rtk) for **60-90% token reduction** before the output reaches your LLM's context window.

It uses `rtk rewrite` — RTK's canonical hook mechanism — to determine how each command should be filtered. Commands RTK knows about get the full token savings; everything else runs directly. The exit code of `rtk rewrite` is honoured too: a command that matches a `permissions.deny` rule in your Claude settings is refused, never executed.

```
MCP Client (Claude Desktop, Cursor, Windsurf, ...)
  → rtk-mcp (this server)
    → rtk rewrite "cargo test"  →  "rtk cargo test"
    → sh -c "rtk cargo test"    ← filtered output (4,849 B → 39 B)
    → returns to LLM

    → rtk rewrite "nmap -sV …"  →  (no RTK filter)
    → sh -c "nmap -sV …"        ← direct execution, full output
    → returns to LLM
```

Without RTK installed, all commands execute directly (no filtering, no token savings). On Windows the command line goes to Git Bash or `cmd.exe` instead of `sh` ([details](#windows-which-shell-runs-the-command)).

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

## Platforms

| | Linux | macOS | Windows |
|---|---|---|---|
| Prebuilt binary | x86_64, arm64 (static, any distro) | Apple silicon, Intel | x86_64 |
| Commands run under | `sh -c` | `sh -c` | Git Bash if Git for Windows is installed, else `cmd.exe` |
| Unit + end-to-end tests in CI | `ubuntu-latest` | `macos-latest` (Apple silicon) | `windows-latest` |

Every push to `main` and every pull request runs the unit tests and an end-to-end test over real MCP stdio (tool surface, RTK routing, deny rules, timeouts and process-tree kill, truncation, the no-RTK allowlist) on all three systems, against RTK 0.34.3, 0.49.0 and 1.1.0 (the standard build) and 1.1.0 `oss`. On Windows it also covers both shells, UTF-8 output and an `rtk.exe` installed next to the server. When a release is built, the binaries that are about to be published run the same test, and the Windows archive is installed with `install.ps1` under Windows PowerShell 5.1 and PowerShell 7.

What that does not cover:

- The Intel macOS build is cross-compiled on an Apple silicon runner and only checked for its architecture; no Intel Mac runs it.
- Nobody has run the Windows build by hand with Claude Desktop on a Windows PC yet. The tests talk to the server the way Claude Desktop does (JSON-RPC over stdio) on a GitHub-hosted Windows runner. If something differs on your machine, please open an issue.
- Windows on Arm is untested (it would run the x64 build under emulation).

## Install

### Windows

Windows 10 or 11, 64-bit. In PowerShell:

```powershell
irm https://raw.githubusercontent.com/admiralhr99/rtk-mcp/main/install.ps1 | iex
```

The script:

- downloads `rtk-mcp.exe` from the latest release and checks its SHA-256 against the release's `checksums.txt`,
- installs [RTK](https://github.com/rtk-ai/rtk) next to it if no working `rtk` is on PATH (same check; RTK 1.1.0, or its Apache-2.0 build with `-RtkEdition oss`),
- adds an `rtk` entry to Claude Desktop's `claude_desktop_config.json`, keeps the other servers in it, and saves the old file next to it as `claude_desktop_config.json.bak-<time>`.

Everything goes under `%LOCALAPPDATA%\Programs\rtk-mcp`; no administrator rights are needed. Then quit Claude Desktop completely (tray icon, Quit), start it again and ask it to run `dir`.

To read the script first, download [install.ps1](install.ps1) and run it with `powershell -ExecutionPolicy Bypass -File .\install.ps1` (PowerShell's default policy refuses to run script files). Options: `-Version v0.6.0`, `-InstallDir`, `-RtkVersion`, `-RtkEdition full|oss`, `-NoConfig` (install only), `-ConfigPath`.

To do it by hand, download `rtk-mcp-x86_64-pc-windows-msvc.zip` and `checksums.txt` from the [latest release](https://github.com/admiralhr99/rtk-mcp/releases/latest), compare `(Get-FileHash .\rtk-mcp-x86_64-pc-windows-msvc.zip).Hash` with its line in `checksums.txt`, unzip, run `Unblock-File` on the exe, install RTK (`winget install rtk-ai.rtk`) and point your client at the exe as shown under [Configure your MCP client](#configure-your-mcp-client).

[Git for Windows](https://gitforwindows.org) is optional but worth having: with it commands run under Git Bash and take the same syntax as on Linux; without it they run under `cmd.exe` (see [which shell runs the command](#windows-which-shell-runs-the-command)).

### macOS and Linux

Download the file for your system from the [latest release](https://github.com/admiralhr99/rtk-mcp/releases/latest):

| System | File |
|--------|------|
| Linux x86_64 | `rtk-mcp-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `rtk-mcp-aarch64-unknown-linux-musl.tar.gz` |
| macOS, Apple silicon | `rtk-mcp-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `rtk-mcp-x86_64-apple-darwin.tar.gz` |

```bash
curl -fsSLO https://github.com/admiralhr99/rtk-mcp/releases/latest/download/rtk-mcp-aarch64-apple-darwin.tar.gz
curl -fsSLO https://github.com/admiralhr99/rtk-mcp/releases/latest/download/checksums.txt
grep rtk-mcp-aarch64-apple-darwin.tar.gz checksums.txt | shasum -a 256 -c -   # Linux: sha256sum -c -
tar xzf rtk-mcp-aarch64-apple-darwin.tar.gz rtk-mcp
```

Each file also has a signed build attestation, so `gh attestation verify rtk-mcp-aarch64-apple-darwin.tar.gz --repo admiralhr99/rtk-mcp` shows which workflow run built it. The binaries are not code-signed or notarized: a file fetched with `curl` runs as is, but one downloaded in a browser carries macOS's quarantine flag until you run `xattr -d com.apple.quarantine rtk-mcp`. The Linux builds are statically linked and run on any distribution.

To build it yourself instead (Rust 1.89 or newer, `rustup update stable`):

```bash
git clone https://github.com/admiralhr99/rtk-mcp.git
cd rtk-mcp
cargo build --release   # the binary is target/release/rtk-mcp (rtk-mcp.exe on Windows)
```

Or run `./install.sh` from a checkout: it downloads RTK if it is missing (SHA-256 verified against the release's `checksums.txt`), builds the checkout it is run from, and adds the server to Claude Desktop's config.

### RTK itself

rtk-mcp needs [RTK](https://github.com/rtk-ai/rtk) for the token savings. The Windows installer fetches it for you; elsewhere install it first (any of its official methods):

```bash
brew install rtk                                  # macOS / Linux
curl -fsSL https://raw.githubusercontent.com/rtk-ai/rtk/refs/heads/master/install.sh | sh
cargo install --git https://github.com/rtk-ai/rtk # from source (not `cargo install rtk` — different crate)
winget install rtk-ai.rtk                         # Windows

# Verify
rtk --version   # Should show "rtk X.Y.Z"
rtk gain        # Should work (not "command not found")
```

rtk-mcp is tested against RTK 0.34.3, 0.49.0 and 1.1.0 on all three systems. From 1.0 RTK ships two builds of the same CLI, the standard `rtk` and the Apache-2.0 `rtk-oss`, and both work: rtk-mcp only needs `rtk rewrite`. Newer RTK adds options to the stats tools (e.g. `view=recalls` needs 0.49+); on an older RTK those return a clear "upgrade RTK" error instead of failing silently. Without RTK the server still runs commands, just unfiltered and behind its allowlist.

RTK is a separate project with its own licence terms, which rtk-mcp neither bundles nor changes. RTK 1.x standard builds carry a second licence file, `LICENSE-ENGINE` (Business Source License 1.1, for RTK's engine crate), next to the Apache-2.0 `LICENSE`; the `oss` build is Apache-2.0 only. Read RTK's licence files if that matters for your use.

## Configure your MCP client

### Claude Desktop

Add to `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS), `~/.config/Claude/claude_desktop_config.json` (Linux) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows). Some Windows installations, the Microsoft Store package for one, keep the file in the package folder instead, `%LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude\`; `install.ps1` uses whichever exists, and Settings, Developer, Edit Config in Claude Desktop opens the right one on any system:

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

On Windows write the path with doubled backslashes, e.g. `"command": "C:\\Users\\you\\AppData\\Local\\Programs\\rtk-mcp\\rtk-mcp.exe"`. Quit Claude Desktop completely and start it again after changing the file.

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

On Linux and macOS commands run under `sh -c` — POSIX sh (dash on Debian/Ubuntu/Mint), not bash. Wrap bash-only syntax (`{1..5}`, `[[ ]]`, `<(...)`, `&>`) in `bash -c '...'`. On Windows they run under Git Bash or `cmd.exe`, see [below](#windows-which-shell-runs-the-command).

Two RTK conventions work through `run_command` as-is:

- `RTK_DISABLED=1 <command>` runs the command raw, with no filtering.
- When RTK elides part of an output it prints a recall hint (`rtk recall <id>`, or a saved-file path in tee mode); run that with `run_command` to get the full text.

### Windows: which shell runs the command

Windows has no `sh`, so the server picks a shell once, at startup, and logs which one:

| `RTK_MCP_SHELL` | Shell |
|-----------------|-------|
| `auto` (default) | Git Bash (`bin\bash.exe` of Git for Windows) if it is installed, else `cmd.exe` |
| `bash` | Git Bash; `cmd.exe`, with a warning in the log, if Git for Windows is not found |
| `cmd` | `cmd.exe` |
| a path, e.g. `C:\msys64\usr\bin\bash.exe` | that POSIX shell, run as `<shell> -c "<command>"` |

`C:\Windows\System32\bash.exe` is never used: it starts WSL, not Git Bash.

The model is told which shell it is writing for (the `run_command` description names it), so it uses the matching syntax. Under Git Bash that is the Linux syntax from above; Windows drive paths work as `C:/dir` or `/c/dir`, and flags of native Windows programs need a doubled slash (`taskkill //F`). Under `cmd.exe` it is `dir`, `type`, `findstr`, `%VAR%`, `&&` and `|` on one line; for PowerShell the model runs `powershell -NoProfile -Command "..."`.

- **No console windows**: commands run without a visible console, so a GUI client such as Claude Desktop does not flash a window per command.
- **UTF-8**: output is read as UTF-8. `cmd.exe` prints its own commands (`dir`, `echo`, `set`) in the console's OEM code page, so the server starts it in a UTF-8 console through a small launcher: the same `rtk-mcp.exe`, started with a hidden argument. While a `cmd.exe` command runs, Task Manager shows a second `rtk-mcp.exe`. Python writes to a pipe in the ANSI code page unless told otherwise, so the server sets `PYTHONIOENCODING=utf-8` for commands unless you set it yourself. Other programs print in whatever encoding they choose.
- **`rtk.exe` next to `rtk-mcp.exe`**: one that sits there (where `install.ps1` puts it when RTK was missing) is used first, and its folder is put at the front of `PATH` for the commands the server runs.

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

**Allowlist** (enforced only when RTK is absent and `RTK_MCP_ALLOW_ALL` is unset): `git`, `cargo`, `npm`, `npx`, `pnpm`, `pytest`, `ruff`, `mypy`, `pip`, `uv`, `go`, `golangci-lint`, `docker`, `grep`, `find`, `ls`, `cat`, `head`, `tail`, `wc`, `env`, `echo`, `pwd`, `gh`, `curl`, `wget`, `node`, `tsc`, `next`, `prettier`, `eslint`, `biome`, `playwright`, `prisma`, `vitest`, `dotnet`, `psql`, `make`, `tree`, `bash`, `sh`, `zsh`, `tar`, `unzip`, `zip`, `nmap`, `nuclei`, `httpx`, `aws`, `kubectl`, and more. On Windows it also has `cmd`, `powershell`, `pwsh`, `dir`, `findstr`, `where`, `ipconfig`, `tasklist`, `hostname` and `systeminfo`; a directory and an `.exe` suffix are ignored when matching, so `C:\Windows\System32\HOSTNAME.EXE` counts as `hostname`.

### Other protections

- **stdin isolation**: Child processes never inherit the MCP server's stdin fd (prevents EAGAIN transport crashes on macOS)
- **Process-tree kill**: every command leads its own process group; on timeout, or when the client cancels the call, the whole tree is SIGKILLed (a timed-out `nmap`/`ffuf` does not keep running behind `sh`). On Windows the command runs inside a job object that is terminated instead; that also catches what Git Bash starts, which `taskkill /T` cannot follow because Git Bash's fork-and-exec leaves those processes without a living parent. The output printed before a timeout comes back with the timeout error, so half a scan is not lost. Commands that exit on their own keep any background jobs they deliberately started
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
                                          │  → 39 B, not 4.8 KB  │
                                          └──────────────────────┘
```

1. MCP client sends a `tools/call` request with a command string
2. `rtk-mcp` calls `rtk rewrite -- "<command>"` (in the command's `cwd`) to get the RTK-optimized form, or a deny verdict
3. Executes the rewritten command (or the original if RTK has no filter) via `sh -c` (Git Bash or `cmd.exe` on Windows)
4. Returns the output with exit code information

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `RTK_MCP_TIMEOUT_SECS` | `300` | Command execution timeout in seconds (a positive number; anything else uses the default) |
| `RTK_MCP_MAX_OUTPUT_KB` | `256` | Largest output returned to the LLM, in KB (a positive number; anything else uses the default) |
| `RTK_MCP_CWD_JAIL` | unset | Directory that every `cwd` must lie inside |
| `RTK_MCP_ALLOW_ALL` | `1` if RTK is installed, else `0` | Bypass the built-in command allowlist |
| `RTK_MCP_SHELL` | `auto` | Windows only: `auto`, `bash`, `cmd` or the path of a POSIX shell, see [which shell runs the command](#windows-which-shell-runs-the-command). Ignored elsewhere |
| `RTK_SUPPRESS_HOOK_WARNING` | `1` for children | Set by the server unless you set it yourself |

The server resolves `rtk` through `PATH`. To pin a specific RTK, put its directory first in `PATH` in your client's `env` block.

## Development

```bash
# Unit tests
cargo test

# Build
cargo build --release

# End-to-end test over real MCP stdio (uses the `rtk` on PATH in a throwaway HOME)
python3 tests/functional.py                    # Windows: python tests\functional.py
python3 tests/functional.py path/to/rtk-mcp    # another binary

# Test MCP protocol manually
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' | ./target/release/rtk-mcp
```

The Windows-specific code is in `src/shell.rs`. Most of it (choosing the shell, quoting a command line for `cmd.exe`, finding Git Bash, matching program names) is plain string and path logic that compiles and is unit-tested on every system; only the few calls into Windows itself (the console code page, job objects) are `cfg(windows)`. Building on Windows needs the MSVC build tools (`rustup` explains how to get them).

CI (`.github/workflows/ci.yml`) runs formatting, clippy, the unit tests, a release build and the end-to-end test on Linux, macOS and Windows, once per RTK version in its matrix. RTK is installed from a pinned release and checked against that release's `checksums.txt` (`.github/actions/install-rtk`).

Releases: push a tag `vX.Y.Z` that matches `Cargo.toml`. `.github/workflows/release.yml` builds the five targets, runs the end-to-end test on the ones it can run, uploads the archives and `checksums.txt`, attests their provenance and installs the Windows archive with `install.ps1` under both PowerShells. A tag with a suffix (`v0.7.0-rc.1`) makes a pre-release.

## Credits

- [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) by [rtk-ai](https://github.com/rtk-ai) — all filtering and rewrite logic
- [rtk-mcp](https://github.com/ousamabenyounes/rtk-mcp) by [Ousama Ben Younes](https://github.com/ousamabenyounes) — the original MCP bridge this project started from (initial server, CI and the macOS installer)
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
- Built with [Claude Code](https://claude.ai/code)

## License

MIT, see [LICENSE](LICENSE).
