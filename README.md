# rtk-mcp

MCP server bridge for [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) — token-optimized CLI output for any MCP-compatible client. Runs on Linux, macOS and Windows.

It installs RTK itself when RTK is missing and upgrades an older RTK to the latest release (the `install_rtk` tool, [details](#rtk-itself)), and it asks the model to use RTK for shell commands. Whether the model does depends on the client and on what else it can run commands with, see [Getting the model to use it](#getting-the-model-to-use-it).

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

What it costs: the tool descriptions and the server instructions are read once per conversation. For the nine tools of 0.8.1 (0.8.0 is the same) that is 10.5 KB (about 2,600 tokens at 4 bytes per token); 0.7.0 had the same nine tools in 10.2 KB, and the four tools of 0.6.0 were 7.0 KB. The `git diff` above saves 121 KB, so one large output pays that back, but a conversation that only runs short commands costs more than it saves. A client that defers MCP tools behind a tool search reads only the tool names until the model looks a tool up; since 0.8.1 `run_command` is the exception (about 1.6 KB of description, plus its schema), see [Getting the model to use it](#getting-the-model-to-use-it).

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

Without RTK installed, commands execute directly (no filtering, no token savings), and the result tells the model it can call `install_rtk`; an RTK that is out of date is reported by `get_rtk_status`, and `install_rtk` upgrades it ([details](#rtk-itself)). On Windows the command line goes to Git Bash or `cmd.exe` instead of `sh` ([details](#windows-which-shell-runs-the-command)).

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

Every push to `main` and every pull request runs the unit tests and an end-to-end test over real MCP stdio (tool surface, RTK routing, deny rules, timeouts and process-tree kill, truncation, the no-RTK allowlist, where RTK is found, and two kinds of real download from GitHub: `install_rtk` installing the latest RTK where there is none, in the jobs whose RTK is the release `install_rtk` pins, and `install_rtk` upgrading an older RTK in place, in every job) on all three systems, against RTK 0.34.3, 0.49.0 and 1.1.0 (the standard build) and 1.1.0 `oss`. On Windows it also covers both shells, UTF-8 output and an `rtk.exe` installed next to the server. When a release is built, the binaries that are about to be published run the same test, and the Windows archive is installed with `install.ps1` under Windows PowerShell 5.1 and PowerShell 7.

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

rtk-mcp needs [RTK](https://github.com/rtk-ai/rtk) for the token savings, but you do not have to install it first, or keep it up to date by hand. Without RTK the server still runs commands, unfiltered, and the result says so; `get_rtk_status` says when the RTK in use is older than the release this rtk-mcp was built with. In both cases the model can call the `install_rtk` tool, which

- asks GitHub which release is the latest (it reads where `github.com/rtk-ai/rtk/releases/latest` redirects to: no API, no token) and downloads that release's archive for your system from `github.com/rtk-ai/rtk` with `curl`, over HTTPS only,
- compares the archive's SHA-256 with the expected one and installs nothing if they differ (see below for where the hash comes from),
- **with no RTK on the machine**, unpacks the single `rtk` executable and writes it to `~/.local/bin` (`%USERPROFILE%\.local\bin` on Windows; `RTK_MCP_INSTALL_DIR` changes it),
- **with an RTK that is older than the latest release**, replaces it where it is (see below),
- **with an RTK that is as new as the latest release, or newer**, does nothing: it never downgrades,

and then uses the result from the next command on, with no restart.

**Upgrading.** The new executable is written next to the old one, run once to see that it works, and only then renamed over the old file, so a failed download, a wrong hash or a broken file leaves the old RTK as it was. rtk-mcp does not touch RTK's settings, hooks or history. Only a plain file in a directory the server can write to is replaced. An RTK behind a symbolic link (what Homebrew and winget create), next to a Scoop `.shim` file, in a read-only directory such as `/usr/bin`, or whose version cannot be read is left alone: `install_rtk` answers with an error that says why and how to update it the way you installed it (`brew upgrade rtk`, `winget upgrade rtk-ai.rtk`, `scoop update rtk`), so the model does not take the job for done. From RTK 1.0 the standard and the Apache-2.0 builds print the same version, so rtk-mcp cannot tell which one it is replacing: an upgrade installs the build `RTK_MCP_RTK_EDITION` names (the standard one by default) and the answer says which.

**Where the hash comes from.** For the release this rtk-mcp was built with (v1.1.0 in 0.8.0) it is compiled into rtk-mcp, copied from RTK's own `checksums.txt` for that release (CI compares the copy with upstream). For a newer release there is nothing compiled in, so it is the `checksums.txt` published with that release. That file comes from the same place as the archive: it catches a damaged or half-uploaded download, or a mirror that serves something else, but not a release that was replaced at the source, which a hash compiled into rtk-mcp does catch. If you want that guarantee for every install, set `RTK_MCP_RTK_VERSION=pinned`: the installer then goes only for the release rtk-mcp carries the hashes of, and a newer RTK arrives with a newer rtk-mcp. Whichever you pick, the answer of `install_rtk` names the release and where the hash came from.

If GitHub cannot be asked which release is the latest (a proxy that blocks that page, no network), `install_rtk` falls back on the release rtk-mcp was built with, and the answer says so.

`get_rtk_status` never goes online: it compares the RTK in use with the release rtk-mcp was built with. A release newer than that is found when `install_rtk` runs, by the model on request or by the server itself at startup with `RTK_MCP_AUTO_INSTALL=1`. The install scripts put RTK on the machine when it is missing too (`install.sh` takes the latest release, `install.ps1` takes v1.1.0 unless `-RtkVersion` says otherwise); `install_rtk` takes it from there.

The tool takes no parameters, so the model cannot pick a URL or a version. It needs `curl` (included with macOS, Windows 10 and later, and most Linux distributions). Prebuilt RTK exists for Linux x86_64 and arm64, macOS and Windows x86_64; on any other system the tool says so and names the manual route. Four settings: `RTK_MCP_NO_INSTALL=1` removes the tool and every mention of it, `RTK_MCP_AUTO_INSTALL=1` makes the server install a missing RTK, or upgrade an old one, by itself at startup in the background (off by default, so nothing is downloaded unless you or the model ask for it), `RTK_MCP_RTK_VERSION=pinned` restricts the installer to the pinned release, and `RTK_MCP_RTK_EDITION=oss` installs the Apache-2.0 build.

An RTK that is already on the machine is found without any of this, also when the client starts the server with a short `PATH` (Claude Desktop often leaves out `~/.local/bin`, `~/.cargo/bin` and Homebrew's directory). The places, in order: `RTK_MCP_RTK_PATH`, the folder of the rtk-mcp executable, the `PATH`, then the usual install directories (`RTK_MCP_INSTALL_DIR` if set, `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin` and Linuxbrew's; Scoop shims and WinGet links on Windows). A file named `rtk` that does not behave like RTK (another program, a damaged file) is skipped, and `get_rtk_status` lists what was skipped.

To install RTK yourself instead (any of its official methods):

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

## Getting the model to use it

Having the server installed is not enough on its own: the model has to choose `run_command` over whatever else it can run commands with. rtk-mcp does what a server can do about that:

- **The `run_command` description opens with the instruction**: "Run a shell command through RTK, which cuts the tokens in its output by 60-90%. Use this for ALL shell commands (git, ls, cat, grep, find, cargo, npm, docker, curl, tests, scripts) instead of any other shell, terminal or command tool." A client that loads the tool's definition shows the model this text. A client that defers MCP tools behind a tool search shows only the name `run_command` until the model looks the tool up, which is why the next point exists.
- **`run_command` asks to be loaded upfront.** Since 0.8.1 the tool carries `"_meta": {"anthropic/alwaysLoad": true}`, the hint Claude Code documents for this ([Scale with MCP tool search](https://code.claude.com/docs/en/mcp#scale-with-mcp-tool-search)): the definition is in the model's context from the first message, so the model does not have to find the tool first. Clients that do not know the key ignore it. Only `run_command` has it; the other eight tools stay deferrable, because every tool loaded upfront costs context in every conversation. Observed, not guaranteed: in a test through the Claude desktop app's remote devices (Linux, 0.8.1), `run_command` still arrived as a bare name in the deferred list and the model ran its commands with Desktop Commander instead, so the hint did not take effect there.
- **The server instructions say it again.** rtk-mcp also sends the rule in the `instructions` field of the MCP handshake. A client that puts that field into the system prompt (Claude Code does) gives the model the rule before the first message. Claude Desktop is reported not to pass the field on ([anthropics/claude-code#43749](https://github.com/anthropics/claude-code/issues/43749), one user's reading of the app, not confirmed by Anthropic), and a session that reached rtk-mcp through a bridge to a computer (the Claude desktop app's remote devices) showed the model no rtk-mcp instructions either. That is why the description carries the rule too.
- **A missing RTK is announced where the model will see it**: the first `run_command` result without RTK, and every 20th after it, ends with a note that names `install_rtk`.

What this cannot do: no server can make a client pick its tool. A description is a strong hint, not a rule, and it only helps once the model has seen it: a model that has another shell tool loaded and knows `run_command` only by name will use the tool it has. When other tools can run commands too (a built-in terminal tool, Desktop Commander, another shell server) the model chooses among them, and in some conversations it will choose another. If you need it to be certain, remove the competing shell tools, or put one line in your own instructions ("Use run_command for shell commands"). If your client defers tools, say so in that line: "Use run_command for shell commands; if only its name is listed, load it first." Do not leave an instruction there that names another shell tool as the default or the only one: the model follows it. Claude Code can do better without relying on the model's choice: RTK's own hook (`rtk init -g`) filters its Bash tool directly.

## Tools

| Tool | Description |
|------|-------------|
| `run_command` | Execute any shell command with automatic RTK filtering |
| `install_rtk` | Install RTK when it is missing, or upgrade an older one in place, to the latest release: SHA-256 verified, used at once ([details](#rtk-itself)) |
| `get_rtk_status` | Which RTK is in use (path, version, how it was found), the mode, the limits, what this RTK version supports, and the installer's plan |
| `get_rtk_savings` | Token savings statistics (`rtk gain`): graph, history, daily/weekly/monthly, failures, recalls, quota, JSON/CSV |
| `get_rtk_discover` | Find missed RTK savings from Claude Code history (`rtk discover`) |
| `get_rtk_session` | Show RTK adoption across recent sessions |
| `get_rtk_recall` | The original text of output RTK shortened (`rtk recall`, RTK 0.49+): by hash, as a list, or a range or regex of it |
| `get_rtk_rewrite` | What RTK would do with a command, without running it: the rewritten form, no filter, or refused by a deny rule |
| `get_rtk_config` | RTK's current configuration (`rtk config`) |

Every tool except `run_command` and `install_rtk` is read-only (`readOnlyHint`), so clients can auto-approve them. `install_rtk` writes one file, and when it upgrades it replaces an existing RTK executable, so it is marked as destructive (and idempotent): a client that asks before running tools should ask for it. `rtk gain --reset` is deliberately not reachable.

RTK's other subcommands are left out on purpose: the ones that change its setup or trust (`init`, `config --create`, `trust`, `untrust`, `telemetry`) and the ones about Claude Code's own hooks, spending or RTK plugin development (`verify`, `learn`, `cc-economics`, `llm-proxy`, `pipeline`, `plugins`). RTK's output filters (`rtk err`, `rtk read`, `rtk test`, ...) need no tool of their own: `rtk` is on the `PATH` of every command, so `run_command` runs them.

With RTK older than 0.49, `get_rtk_recall` answers with the version it needs instead of running: RTK takes an unknown subcommand for a program to execute.

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

### `get_rtk_recall` parameters

| Parameter | Type | Description |
|-----------|------|-------------|
| `hash` | string | Hash from an RTK recall hint (a unique prefix is enough). Not needed with `list` |
| `list` | bool | List the stored entries instead of recalling one |
| `full` | bool | The complete output, not just the part RTK left out |
| `from` | integer | Start at this 1-based line of the full output |
| `lines` | integer | Only the first N lines of the full output |
| `grep` | string | Only the lines matching this regex |

### `get_rtk_rewrite` parameters

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `command` | string | Yes | The command to preview, as you would pass it to `run_command`. It is never run |
| `cwd` | string | No | Working directory; project-level deny rules are looked up from here. Same rules as for `run_command` |

`install_rtk`, `get_rtk_status` and `get_rtk_config` take no parameters.

Example tool calls from the LLM:

```json
{"name": "run_command", "arguments": {"command": "git log --oneline -5", "cwd": "/my/project"}}
{"name": "run_command", "arguments": {"command": "cargo test | grep FAILED"}}
{"name": "run_command", "arguments": {"command": "ls -la src/"}}
{"name": "run_command", "arguments": {"command": "nmap -sV 10.0.0.1"}}
{"name": "get_rtk_recall", "arguments": {"hash": "a1b2c3", "grep": "error"}}
{"name": "get_rtk_rewrite", "arguments": {"command": "git status && cargo build"}}
```

## Security

### Command filtering

When RTK is available, `rtk rewrite` is the gate. Its exit code decides: `1` means RTK has no filter (the command runs as given), `0`/`3` mean rewritten (run the rewrite), and `2` means a `permissions.deny` rule in your Claude settings (`~/.claude/settings.json`, or the project's `.claude/settings.json` found from `cwd`) matches — the command is refused and never executed. `allow`/`ask` rules are not enforced: your MCP client's approval prompt for `run_command` is the gate for those. When RTK is absent and `RTK_MCP_ALLOW_ALL` is not set, the built-in allowlist is enforced.

The allowlist ends when RTK appears. With RTK present, installed by hand or through `install_rtk` while the server runs, any command may run, and the deny rules plus your client's approval prompt are the gates. The allowlist is a speed bump rather than a sandbox anyway (it includes `bash`, `sh` and `zsh`), but if you rely on it, set `RTK_MCP_NO_INSTALL=1` so the model cannot install RTK and so lift it.

**Allowlist** (enforced only when RTK is absent and `RTK_MCP_ALLOW_ALL` is unset): `git`, `cargo`, `npm`, `npx`, `pnpm`, `pytest`, `ruff`, `mypy`, `pip`, `uv`, `go`, `golangci-lint`, `docker`, `grep`, `find`, `ls`, `cat`, `head`, `tail`, `wc`, `env`, `echo`, `pwd`, `gh`, `curl`, `wget`, `node`, `tsc`, `next`, `prettier`, `eslint`, `biome`, `playwright`, `prisma`, `vitest`, `dotnet`, `psql`, `make`, `tree`, `bash`, `sh`, `zsh`, `tar`, `unzip`, `zip`, `nmap`, `nuclei`, `httpx`, `aws`, `kubectl`, and more. On Windows it also has `cmd`, `powershell`, `pwsh`, `dir`, `findstr`, `where`, `ipconfig`, `tasklist`, `hostname` and `systeminfo`; a directory and an `.exe` suffix are ignored when matching, so `C:\Windows\System32\HOSTNAME.EXE` counts as `hostname`.

### Other protections

- **stdin isolation**: Child processes never inherit the MCP server's stdin fd (prevents EAGAIN transport crashes on macOS)
- **Process-tree kill**: every command leads its own process group; on timeout, or when the client cancels the call, the whole tree is SIGKILLed (a timed-out `nmap`/`ffuf` does not keep running behind `sh`). On Windows the command runs inside a job object that is terminated instead; that also catches what Git Bash starts, which `taskkill /T` cannot follow because Git Bash's fork-and-exec leaves those processes without a living parent. The output printed before a timeout comes back with the timeout error, so half a scan is not lost. Commands that exit on their own keep any background jobs they deliberately started
- **Background jobs**: a job that was started with its output still attached (`ssh -fN …`, `nohup server &`, `sleep 30 &`) does not hold the call until the timeout. Once the command itself has exited its output gets about 2 seconds to arrive, the call returns with a note, and the job keeps running; whatever it prints later is discarded (read and dropped, so it can neither block on a full pipe nor die of SIGPIPE). Redirect to a file (`cmd > out.log 2>&1 &`) and the call returns at once, without the note
- **Bounded memory**: at most 4× the output cap (1MB by default) is held per stream while a command runs (the rest is drained), and 256KB is returned (`RTK_MCP_MAX_OUTPUT_KB`) — truncation is UTF-8 safe
- **Working-directory jail**: set `RTK_MCP_CWD_JAIL=/some/root` and a `cwd` outside it (after `..` and symlinks are resolved) is refused before anything runs. It guards the `cwd` parameter only — it is not a sandbox: the command itself can still `cd` elsewhere or name absolute paths
- **Quiet children**: `RTK_SUPPRESS_HOOK_WARNING=1` is set for child processes (unless you set it) so RTK's daily "no hook installed" nag never reaches the LLM
- **Installer**: `install_rtk` takes no parameters. It talks to `github.com/rtk-ai/rtk` only (over HTTPS with `curl`, TLS 1.2 or newer, 100 MB and 150 s limits): the page that names the latest release, that release's `checksums.txt` and its archive for your system. The tag it reads off the redirect has to be a plain `vN.N.N` of that repository, or the lookup counts as failed (GitHub's page for the latest release never names a pre-release). It refuses an archive whose SHA-256 is not the expected one (compiled into rtk-mcp for the pinned release, from the release's own `checksums.txt` for a newer one, see [RTK itself](#rtk-itself) for what that does and does not prove), unpacks one file (the `rtk` executable, at most 128 MiB) and writes it through a temporary file and a rename. The only thing it runs is the new file, as `rtk --version` and a test `rtk rewrite`, before it replaces anything: a file that is not a working RTK, or one older than the RTK it would replace, is removed and the old one stays
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
| `RTK_MCP_ALLOW_ALL` | `1` if RTK is installed, else `0` | Bypass the built-in command allowlist. It only matters while RTK is missing |
| `RTK_MCP_SHELL` | `auto` | Windows only: `auto`, `bash`, `cmd` or the path of a POSIX shell, see [which shell runs the command](#windows-which-shell-runs-the-command). Ignored elsewhere |
| `RTK_MCP_RTK_PATH` | unset | The `rtk` executable to use, tried before every other place. One that is not a working RTK is skipped and reported by `get_rtk_status` |
| `RTK_MCP_NO_INSTALL` | unset | `1`: never download anything; the `install_rtk` tool and every mention of it are left out |
| `RTK_MCP_AUTO_INSTALL` | unset | `1`: at startup, in the background, install RTK when none is found and upgrade the one in use when it is older than the latest release (what `install_rtk` does, without being asked) |
| `RTK_MCP_RTK_VERSION` | `latest` | `latest`: `install_rtk` goes for the latest release. `pinned`: only for the release this rtk-mcp carries the hashes of (v1.1.0 in 0.8.0). Any other value makes the installer refuse and `get_rtk_status` say so |
| `RTK_MCP_INSTALL_DIR` | `~/.local/bin` (`%USERPROFILE%\.local\bin`) | Where a first `install_rtk` puts `rtk`; it is searched like the other usual directories. An upgrade replaces the RTK where it is |
| `RTK_MCP_RTK_EDITION` | `full` | `oss`: install the Apache-2.0 build of RTK (`rtk-oss`), also when upgrading |
| `RTK_SUPPRESS_HOOK_WARNING` | `1` for children | Set by the server unless you set it yourself |

To pin a specific RTK, set `RTK_MCP_RTK_PATH` in your client's `env` block. Where RTK is looked for otherwise is under [RTK itself](#rtk-itself).

## Development

```bash
# Unit tests
cargo test

# Build
cargo build --release

# End-to-end test over real MCP stdio (uses the `rtk` on PATH in a throwaway HOME)
python3 tests/functional.py                    # Windows: python tests\functional.py
python3 tests/functional.py path/to/rtk-mcp    # another binary
RTK_MCP_TEST_NETWORK=1 python3 tests/functional.py   # also downloads RTK from GitHub, to test install_rtk (install, upgrade, latest) for real

# Test MCP protocol manually
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1"}}}' | ./target/release/rtk-mcp
```

The Windows-specific code is in `src/shell.rs`. Most of it (choosing the shell, quoting a command line for `cmd.exe`, finding Git Bash, matching program names) is plain string and path logic that compiles and is unit-tested on every system; only the few calls into Windows itself (the console code page, job objects) are `cfg(windows)`. Building on Windows needs the MSVC build tools (`rustup` explains how to get them).

CI (`.github/workflows/ci.yml`) runs formatting, clippy, the unit tests, a release build and the end-to-end test on Linux, macOS and Windows, once per RTK version in its matrix. RTK is installed from a pinned release and checked against that release's `checksums.txt` (`.github/actions/install-rtk`). The download tests are off on your machine unless you set `RTK_MCP_TEST_NETWORK=1` (`=0` rules them out everywhere). On GitHub Actions the upgrade tests run in every job (an older RTK, a copy of the job's own where that is older than the pinned release, is replaced through `install_rtk`), and the install tests run by themselves in the jobs whose RTK is the release `install_rtk` pins: `install_rtk` downloads the pinned release and the latest one, and the hashes compiled into rtk-mcp (`data/rtk-checksums-<tag>.txt`) are compared with RTK's published `checksums.txt`. Where GitHub's page for the latest release cannot be read (a sandbox proxy), the tests expect the pinned release instead and say so; on GitHub Actions that is a failure. The release `install_rtk` falls back on, and the one `RTK_MCP_RTK_VERSION=pinned` selects, only moves when you change the tag in `src/install.rs`, put that release's `checksums.txt` at `data/rtk-checksums-<tag>.txt` and add the version to the CI matrix; `install_rtk` itself goes for the latest release without any of that.

Releases: push a tag `vX.Y.Z` that matches `Cargo.toml`. `.github/workflows/release.yml` builds the five targets, runs the end-to-end test on the ones it can run, uploads the archives and `checksums.txt`, attests their provenance and installs the Windows archive with `install.ps1` under both PowerShells. A tag with a suffix (`v0.7.0-rc.1`) makes a pre-release.

## Credits

- [RTK (Rust Token Killer)](https://github.com/rtk-ai/rtk) by [rtk-ai](https://github.com/rtk-ai) — all filtering and rewrite logic
- [rtk-mcp](https://github.com/ousamabenyounes/rtk-mcp) by [Ousama Ben Younes](https://github.com/ousamabenyounes) — the original MCP bridge this project started from (initial server, CI and the macOS installer)
- [rmcp](https://github.com/modelcontextprotocol/rust-sdk) — Rust MCP SDK
- Built with [Claude Code](https://claude.ai/code)

## License

MIT, see [LICENSE](LICENSE).
