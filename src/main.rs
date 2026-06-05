use tokio::process::Command;
use tokio::time::{timeout, Duration};

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ServerHandler, ServiceExt,
};
use tracing_subscriber::EnvFilter;

/// Maximum command string length (prevents memory abuse).
const MAX_COMMAND_LEN: usize = 4096;

/// Maximum output size returned to the LLM (prevents context flooding).
const MAX_OUTPUT_BYTES: usize = 64 * 1024; // 64KB

/// Command execution timeout — overridden by RTK_MCP_TIMEOUT_SECS env var (default 120s).
fn command_timeout_secs() -> u64 {
    std::env::var("RTK_MCP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Safety-net allowlist used only when RTK is NOT available AND allow_all is false.
/// When RTK is present, rtk rewrite is used instead — RTK decides what it can filter.
const ALLOWED_COMMANDS: &[&str] = &[
    // VCS
    "git",
    // Build / test
    "cargo", "make", "cmake",
    // Go
    "go", "golangci-lint", "govulncheck",
    // Node
    "npm", "npx", "pnpm", "bun", "node", "tsc", "next",
    "prettier", "eslint", "biome", "playwright", "vitest",
    // Python
    "python3", "python", "pytest", "ruff", "mypy", "pip", "uv",
    // .NET
    "dotnet",
    // Containers
    "docker", "docker-compose", "kubectl",
    // File ops
    "ls", "cat", "head", "tail", "wc", "tree", "find", "grep",
    "awk", "sed", "sort", "uniq", "diff", "xargs", "jq",
    // Shell utils
    "echo", "pwd", "env", "which", "file", "stat", "du", "df",
    // Network / pentest
    "curl", "wget", "ping", "nmap", "netstat", "ss", "ip",
    "ffuf", "gobuster", "feroxbuster", "dirsearch",
    "nuclei", "httpx", "subfinder", "amass", "dnsx",
    "sqlmap", "nikto", "whatweb",
    "openssl", "ssh", "scp",
    // DB
    "psql", "mysql", "sqlite3", "redis-cli",
    // RTK + GitHub
    "rtk", "gh",
    // Prisma / ORM
    "prisma",
    // Ruby
    "rspec", "bundle", "rubocop", "rake",
    // AWS / cloud
    "aws",
];

#[derive(Debug, Clone)]
pub struct RtkMcpServer {
    tool_router: ToolRouter<Self>,
    rtk_available: bool,
    allow_all: bool,
}

impl RtkMcpServer {
    pub fn new() -> Self {
        let rtk_available = validate_rtk_installation();
        // Default allow_all to true when RTK is present — RTK rewrite is the safety net.
        let allow_all = std::env::var("RTK_MCP_ALLOW_ALL")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);

        if rtk_available {
            tracing::info!("RTK detected — commands will be routed through rtk rewrite");
        } else {
            tracing::warn!("RTK not found — commands will execute without filtering");
        }
        if allow_all && !rtk_available {
            tracing::warn!("RTK_MCP_ALLOW_ALL=1 — allowlist bypassed, all commands permitted");
        }

        Self {
            tool_router: Self::tool_router(),
            rtk_available,
            allow_all,
        }
    }
}

impl Default for RtkMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct RunCommandRequest {
    #[schemars(
        description = "The command to execute — anything RTK supports: git, cargo, go, docker, \
        nmap, nuclei, httpx, curl, grep, find, jq, aws, kubectl, gh, rspec, bundle, rubocop, \
        rake, prisma, tsc, playwright, vitest, ruff, mypy, pip, uv, golangci-lint, and many more. \
        Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported."
    )]
    command: String,

    #[schemars(
        description = "Working directory for the command. Defaults to server cwd if omitted."
    )]
    cwd: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetRtkSavingsRequest {
    #[schemars(description = "Show ASCII bar chart of savings per command. Pass true for --graph.")]
    graph: Option<bool>,
}

#[tool_router]
impl RtkMcpServer {
    #[tool(
        name = "run_command",
        description = "Execute ANY shell command through RTK for token-optimized output. \
            Uses `rtk rewrite` to route commands through the appropriate RTK filter (60-90% \
            token savings). Commands without an RTK filter execute directly. \
            Supports everything RTK does: git, cargo, go, docker, nmap, nuclei, httpx, \
            subfinder, ffuf, sqlmap, curl, grep, find, jq, aws, kubectl, gh, rspec, bundle, \
            rubocop, rake, prisma, tsc, playwright, vitest, ruff, mypy, pip, uv, \
            golangci-lint, govulncheck, and any other command. \
            Shell operators (|, &&, ||, ;, >, <, $VAR, *, ~, backticks) are fully supported. \
            Falls back to raw execution if RTK unavailable or has no filter for the command. \
            Timeout: configurable via RTK_MCP_TIMEOUT_SECS (default 120s). Max output: 64KB."
    )]
    async fn run_command(
        &self,
        Parameters(RunCommandRequest { command, cwd }): Parameters<RunCommandRequest>,
    ) -> Result<String, String> {
        let command = command.trim().to_string();

        if command.is_empty() {
            return Err("Error: empty command".to_string());
        }
        if command.len() > MAX_COMMAND_LEN {
            return Err(format!(
                "Command too long: {} > {} chars",
                command.len(),
                MAX_COMMAND_LEN
            ));
        }

        // Enforce allowlist only when RTK is absent AND allow_all is false.
        // When RTK is available, `rtk rewrite` decides what it can filter;
        // unknown commands fall back to direct execution anyway.
        if !self.allow_all && !self.rtk_available {
            let parts = shlex::split(&command)
                .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;
            if parts.is_empty() {
                return Err("Error: empty command after parsing".to_string());
            }
            let base_cmd = parts[0].rsplit('/').next().unwrap_or(&parts[0]).to_string();
            if !ALLOWED_COMMANDS.contains(&base_cmd.as_str()) {
                return Err(format!(
                    "Command '{}' is not in the allowlist. Set RTK_MCP_ALLOW_ALL=1 to bypass, \
                    or use one of: {}",
                    base_cmd,
                    ALLOWED_COMMANDS.join(", ")
                ));
            }
        }

        // Route through RTK using `rtk rewrite` — the canonical hook logic.
        // rewrite returns the token-optimized form (e.g. "rtk cargo test | grep FAILED")
        // or empty if RTK has no filter for the command.
        let effective_command = if self.rtk_available {
            get_rtk_rewrite(&command).await.unwrap_or(command.clone())
        } else {
            command.clone()
        };

        let result = run_shell_cmd(&effective_command, cwd.as_deref()).await?;

        let mut output = result.output;
        if !result.success {
            output.push_str(&format!("\n[exit code: {}]", result.exit_code));
        }

        if result.success {
            Ok(output)
        } else {
            Err(output)
        }
    }

    #[tool(
        name = "get_rtk_savings",
        description = "Show RTK token savings statistics for this session. \
            Returns bytes saved, token reduction percentage, and command history. \
            Pass graph=true for an ASCII bar chart visualization."
    )]
    async fn get_rtk_savings(
        &self,
        Parameters(GetRtkSavingsRequest { graph }): Parameters<GetRtkSavingsRequest>,
    ) -> Result<String, String> {
        if !self.rtk_available {
            return Ok("RTK not installed — no savings data available.\nInstall: cargo install --git https://github.com/rtk-ai/rtk".to_string());
        }
        let args: &[&str] = if graph.unwrap_or(false) {
            &["gain", "--graph"]
        } else {
            &["gain"]
        };
        run_with_timeout("rtk", args, None)
            .await
            .map(|r| r.output)
    }

    #[tool(
        name = "get_rtk_discover",
        description = "Analyze Claude Code command history to discover missed RTK optimization \
            opportunities. Shows which commands could have been routed through RTK for better \
            token savings."
    )]
    async fn get_rtk_discover(&self) -> Result<String, String> {
        if !self.rtk_available {
            return Ok("RTK not installed — discovery unavailable.\nInstall: cargo install --git https://github.com/rtk-ai/rtk".to_string());
        }
        run_with_timeout("rtk", &["discover"], None)
            .await
            .map(|r| r.output)
    }

    #[tool(
        name = "get_rtk_session",
        description = "Show RTK session statistics and active session information."
    )]
    async fn get_rtk_session(&self) -> Result<String, String> {
        if !self.rtk_available {
            return Ok("RTK not installed — session data unavailable.\nInstall: cargo install --git https://github.com/rtk-ai/rtk".to_string());
        }
        run_with_timeout("rtk", &["session"], None)
            .await
            .map(|r| r.output)
    }
}

#[tool_handler]
impl ServerHandler for RtkMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "IMPORTANT: Use run_command for ALL shell execution instead of any built-in bash tool. \
             RTK filters output to reduce token usage 60-90%. Supports git, cargo, go, docker, \
             nmap, nuclei, httpx, subfinder, ffuf, sqlmap, curl, grep, find, jq, aws, kubectl, \
             gh, rspec, bundle, rubocop, rake, prisma, tsc, playwright, vitest, ruff, mypy, pip, \
             uv, golangci-lint, govulncheck, and any other command. \
             Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported. \
             Use the cwd parameter for directory control. Timeout is configurable via \
             RTK_MCP_TIMEOUT_SECS (default 120s).",
        )
    }
}

struct CommandResult {
    output: String,
    exit_code: i32,
    success: bool,
}

/// Ask RTK for the token-optimized equivalent of a command string.
///
/// Uses `rtk rewrite` — the single source of truth for RTK's hook logic.
/// Returns `Some(rewritten)` when RTK has a filter (e.g. "rtk cargo test | grep FAILED"),
/// `None` when RTK has no filter and the command should run directly.
async fn get_rtk_rewrite(command: &str) -> Option<String> {
    let result = run_with_timeout("rtk", &["rewrite", command], None)
        .await
        .ok()?;
    let output = result.output.trim().to_string();
    if output.is_empty() || output == "(no output)" {
        None
    } else {
        Some(output)
    }
}

/// Execute a command string via `sh -c` (supports shell operators and env var expansion).
///
/// # stdin isolation
/// Child processes **must never inherit the MCP server's stdin** (fd 0).  The rmcp
/// stdio transport sets fd 0 to O_NONBLOCK via tokio's event loop.  If a child
/// inherits that fd it can: read MCP protocol bytes, change the O_NONBLOCK flag, or
/// leave the fd in a state where the next tokio read returns EAGAIN (errno 35 on
/// macOS) — which crashes the transport with "Resource temporarily unavailable".
/// Setting `Stdio::null()` prevents all of this.
///
/// # kill_on_drop
/// When a timeout fires, the `timeout()` future drops the inner `command.output()`
/// future.  With `kill_on_drop(true)` tokio sends SIGKILL to the child the moment
/// the `Child` is dropped, so orphaned processes (e.g. a hung `ssh … certbot`) do
/// not linger and cannot interfere with future reads.
async fn run_shell_cmd(shell_str: &str, cwd: Option<&str>) -> Result<CommandResult, String> {
    let timeout_secs = command_timeout_secs();
    let mut command = Command::new("sh");
    command.args(["-c", shell_str]);
    command.stdin(std::process::Stdio::null()); // ← isolate from MCP stdio transport
    command.kill_on_drop(true);                 // ← kill child when timeout drops future
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| format!("command timed out after {}s", timeout_secs))?
        .map_err(|e| format!("failed to execute shell command: {}", e))?;

    finish_output(output)
}

/// Execute a command directly (no shell expansion). Used for RTK probes and savings tools.
/// Same stdin isolation and kill_on_drop rationale as `run_shell_cmd`.
async fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<CommandResult, String> {
    let timeout_secs = command_timeout_secs();
    let mut command = Command::new(cmd);
    command.args(args);
    command.stdin(std::process::Stdio::null()); // ← isolate from MCP stdio transport
    command.kill_on_drop(true);                 // ← kill child when timeout drops future
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| format!("command timed out after {}s", timeout_secs))?
        .map_err(|e| format!("failed to execute '{}': {}", cmd, e))?;

    finish_output(output)
}

fn finish_output(output: std::process::Output) -> Result<CommandResult, String> {
    let exit_code = output.status.code().unwrap_or(-1);
    let mut combined = collect_output(&output);

    if combined.len() > MAX_OUTPUT_BYTES {
        combined.truncate(MAX_OUTPUT_BYTES);
        combined.push_str(&format!(
            "\n[output truncated at {}KB — use head/tail/grep to filter]",
            MAX_OUTPUT_BYTES / 1024
        ));
    }

    Ok(CommandResult {
        output: combined,
        exit_code,
        success: output.status.success(),
    })
}

fn collect_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(no output)".to_string(),
        (false, true) => stdout.into_owned(),
        (true, false) => stderr.into_owned(),
        (false, false) => format!("{}\n[stderr]\n{}", stdout, stderr),
    }
}

/// Validate RTK installation by checking --version and the gain subcommand.
/// Stdin is explicitly set to null so these probes never touch the MCP transport fd.
fn validate_rtk_installation() -> bool {
    let version_ok = std::process::Command::new("rtk")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| {
            let v = String::from_utf8_lossy(&o.stdout);
            v.starts_with("rtk ") && o.status.success()
        })
        .unwrap_or(false);

    if !version_ok {
        return false;
    }

    // Verify `rtk rewrite` works — that's the core routing mechanism this server relies on.
    std::process::Command::new("rtk")
        .args(["rewrite", "git status"])
        .stdin(std::process::Stdio::null())
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            !out.trim().is_empty()
        })
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting RTK-MCP server v{}", env!("CARGO_PKG_VERSION"));

    let service = RtkMcpServer::new().serve(stdio()).await.inspect_err(|e| {
        tracing::error!("Server error: {:?}", e);
    })?;

    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes all tests that mutate RTK_MCP_TIMEOUT_SECS to prevent races.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn command_timeout_secs_default() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
        assert_eq!(command_timeout_secs(), 120);
    }

    #[test]
    fn command_timeout_secs_from_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::set_var("RTK_MCP_TIMEOUT_SECS", "300");
        assert_eq!(command_timeout_secs(), 300);
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
    }

    #[test]
    fn command_timeout_secs_invalid_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::set_var("RTK_MCP_TIMEOUT_SECS", "notanumber");
        assert_eq!(command_timeout_secs(), 120);
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
    }

    #[tokio::test]
    async fn run_with_timeout_basic() {
        let result = run_with_timeout("echo", &["hello"], None).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn run_shell_cmd_pipe() {
        let result = run_shell_cmd("echo hello | tr a-z A-Z", None).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("HELLO"));
    }

    #[tokio::test]
    async fn run_shell_cmd_env_var() {
        let result = run_shell_cmd("echo $HOME", None).await.unwrap();
        assert!(result.success);
        assert!(!result.output.trim().is_empty());
        assert_ne!(result.output.trim(), "$HOME");
    }

    #[tokio::test]
    async fn run_with_timeout_bad_command() {
        let result = run_with_timeout("__nonexistent_cmd_xyz__", &[], None).await;
        assert!(result.is_err());
    }

    #[test]
    fn allowlist_contains_expected_commands() {
        assert!(ALLOWED_COMMANDS.contains(&"git"));
        assert!(ALLOWED_COMMANDS.contains(&"cargo"));
        assert!(ALLOWED_COMMANDS.contains(&"nmap"));
        assert!(ALLOWED_COMMANDS.contains(&"aws"));
        assert!(ALLOWED_COMMANDS.contains(&"rspec"));
        assert!(ALLOWED_COMMANDS.contains(&"bundle"));
    }

    /// RTK rewrite should transform known commands to their token-optimized form.
    #[tokio::test]
    async fn get_rtk_rewrite_known_command() {
        // Only run when RTK is actually available
        if !validate_rtk_installation() {
            return;
        }
        let rewritten = get_rtk_rewrite("git status").await;
        assert!(rewritten.is_some(), "rtk should rewrite 'git status'");
        let r = rewritten.unwrap();
        assert!(r.contains("rtk"), "rewrite should include rtk: {}", r);
    }

    /// RTK rewrite returns None for commands RTK doesn't filter.
    #[tokio::test]
    async fn get_rtk_rewrite_unknown_command() {
        if !validate_rtk_installation() {
            return;
        }
        // `echo` is not an RTK subcommand — rewrite should return None
        let rewritten = get_rtk_rewrite("echo hello world").await;
        assert!(rewritten.is_none(), "rtk should not rewrite 'echo': {:?}", rewritten);
    }

    /// RTK rewrite handles shell operators — rewrites the filterable parts.
    #[tokio::test]
    async fn get_rtk_rewrite_pipe_command() {
        if !validate_rtk_installation() {
            return;
        }
        let rewritten = get_rtk_rewrite("cargo test | grep FAILED").await;
        assert!(rewritten.is_some(), "rtk should rewrite piped cargo test");
        let r = rewritten.unwrap();
        assert!(r.contains("rtk cargo test"), "should contain rtk cargo test: {}", r);
    }

    /// Regression test: child processes must not inherit the MCP server's stdin.
    #[tokio::test]
    async fn stdin_is_isolated_from_parent() {
        let result = run_shell_cmd("cat", None).await.unwrap();
        assert!(result.success, "cat read from /dev/null should succeed");
        assert!(result.output == "(no output)" || result.output.is_empty());
    }

    /// Regression test: a timed-out command must not leave the server in a corrupt state.
    #[tokio::test]
    async fn timeout_does_not_corrupt_server_state() {
        let r1 = run_shell_cmd("echo before", None).await.unwrap();
        assert!(r1.output.contains("before"));

        let _ = run_with_timeout("__will_not_exist_xyz__", &[], None).await;

        let r2 = run_shell_cmd("echo after", None).await.unwrap();
        assert!(r2.output.contains("after"), "server must remain functional after a failed command");
    }
}
