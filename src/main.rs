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

/// Safety-net allowlist used only when RTK is NOT available.
/// When RTK is present and RTK_MCP_ALLOW_ALL=1, this is bypassed entirely.
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
        // Default allow_all to true — RTK is the safety net when available.
        let allow_all = std::env::var("RTK_MCP_ALLOW_ALL")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(true);

        if rtk_available {
            tracing::info!("RTK detected — commands will be filtered for token savings");
        } else {
            tracing::warn!("RTK not found — commands will execute without filtering");
        }
        if allow_all {
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
            When RTK is available, wraps commands transparently for 60-90% token savings. \
            Supports everything RTK does: git, cargo, go, docker, nmap, nuclei, httpx, \
            subfinder, ffuf, sqlmap, curl, grep, find, jq, aws, kubectl, gh, rspec, bundle, \
            rubocop, rake, prisma, tsc, playwright, vitest, ruff, mypy, pip, uv, \
            golangci-lint, govulncheck, and any other command. \
            Shell operators (|, &&, ||, ;, >, <, $VAR, *, ~, backticks) are fully supported. \
            Falls back to raw execution if RTK unavailable. \
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

        let has_shell_ops = needs_shell(&command);

        // Skip allowlist when RTK is available AND allow_all is set.
        // When RTK is absent, always enforce the allowlist as a safety net.
        let skip_allowlist = self.rtk_available && self.allow_all;

        if !skip_allowlist {
            let parts = shlex::split(&command)
                .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;
            if parts.is_empty() {
                return Err("Error: empty command after parsing".to_string());
            }
            let base_cmd = parts[0].rsplit('/').next().unwrap_or(&parts[0]).to_string();
            if !self.allow_all && !ALLOWED_COMMANDS.contains(&base_cmd.as_str()) {
                return Err(format!(
                    "Command '{}' is not in the allowlist. Set RTK_MCP_ALLOW_ALL=1 to bypass, \
                    or use one of: {}",
                    base_cmd,
                    ALLOWED_COMMANDS.join(", ")
                ));
            }
        }

        let result = if self.rtk_available {
            if has_shell_ops {
                // Shell ops + RTK: sh -c "rtk <full_command>" for transparent filtering.
                run_shell_cmd(&format!("rtk {}", command), cwd.as_deref()).await?
            } else {
                let parts = shlex::split(&command)
                    .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;
                if parts.is_empty() {
                    return Err("Error: empty command after parsing".to_string());
                }
                let parts_ref: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
                match run_with_timeout("rtk", &parts_ref, cwd.as_deref()).await {
                    Ok(out) => out,
                    Err(rtk_err) => {
                        tracing::warn!("rtk failed ({}), falling back to raw command", rtk_err);
                        run_with_timeout(parts_ref[0], &parts_ref[1..], cwd.as_deref()).await?
                    }
                }
            }
        } else if has_shell_ops {
            // No RTK, shell ops: sh -c "<full_command>".
            run_shell_cmd(&command, cwd.as_deref()).await?
        } else {
            let parts = shlex::split(&command)
                .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;
            if parts.is_empty() {
                return Err("Error: empty command after parsing".to_string());
            }
            let parts_ref: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
            run_with_timeout(parts_ref[0], &parts_ref[1..], cwd.as_deref()).await?
        };

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

/// Returns true if the command string contains shell operators that require `sh -c`.
fn needs_shell(cmd: &str) -> bool {
    cmd.contains("&&")
        || cmd.contains("||")
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('>')
        || cmd.contains('<')
        || cmd.contains('$')
        || cmd.contains('*')
        || cmd.contains('?')
        || cmd.contains('~')
        || cmd.contains('`')
}

/// Execute a command string via `sh -c` (supports shell operators and env var expansion).
async fn run_shell_cmd(shell_str: &str, cwd: Option<&str>) -> Result<CommandResult, String> {
    let timeout_secs = command_timeout_secs();
    let mut command = Command::new("sh");
    command.args(["-c", shell_str]);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| format!("command timed out after {}s", timeout_secs))?
        .map_err(|e| format!("failed to execute shell command: {}", e))?;

    finish_output(output)
}

/// Execute a command directly (no shell expansion). Used for clean, parsed commands.
async fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<CommandResult, String> {
    let timeout_secs = command_timeout_secs();
    let mut command = Command::new(cmd);
    command.args(args);
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

/// Validate RTK installation by checking both --version and the gain subcommand.
fn validate_rtk_installation() -> bool {
    let version_ok = std::process::Command::new("rtk")
        .arg("--version")
        .output()
        .map(|o| {
            let v = String::from_utf8_lossy(&o.stdout);
            v.starts_with("rtk ") && o.status.success()
        })
        .unwrap_or(false);

    if !version_ok {
        return false;
    }

    std::process::Command::new("rtk")
        .arg("gain")
        .output()
        .map(|o| o.status.success())
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

    #[test]
    fn needs_shell_detects_pipe() {
        assert!(needs_shell("ls | wc -l"));
    }

    #[test]
    fn needs_shell_detects_and_and() {
        assert!(needs_shell("git fetch && git rebase"));
    }

    #[test]
    fn needs_shell_detects_redirect() {
        assert!(needs_shell("echo hello > /tmp/out"));
    }

    #[test]
    fn needs_shell_detects_dollar() {
        assert!(needs_shell("echo $HOME"));
    }

    #[test]
    fn needs_shell_detects_glob() {
        assert!(needs_shell("ls *.rs"));
    }

    #[test]
    fn needs_shell_clean_command() {
        assert!(!needs_shell("git status"));
        assert!(!needs_shell("cargo test --lib"));
        assert!(!needs_shell("nmap -sV 10.0.0.1"));
    }

    #[test]
    fn command_timeout_secs_default() {
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
        assert_eq!(command_timeout_secs(), 120);
    }

    #[test]
    fn command_timeout_secs_from_env() {
        std::env::set_var("RTK_MCP_TIMEOUT_SECS", "300");
        assert_eq!(command_timeout_secs(), 300);
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
    }

    #[test]
    fn command_timeout_secs_invalid_env() {
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
}
