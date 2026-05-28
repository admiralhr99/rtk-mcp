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

/// Command execution timeout in seconds.
const COMMAND_TIMEOUT_SECS: u64 = 120;

/// Commands allowed to execute. Set RTK_MCP_ALLOW_ALL=1 to bypass for dev use.
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
        let allow_all = std::env::var("RTK_MCP_ALLOW_ALL")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(false);

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
        description = "The command to execute, e.g. 'git status', 'cargo test', 'nmap -sV 10.0.0.1'. \
        Allowlisted: git, cargo, go, docker, nmap, nuclei, httpx, curl, grep, find, ls, jq, etc."
    )]
    command: String,

    #[schemars(
        description = "Working directory for the command. Defaults to server cwd if omitted."
    )]
    cwd: Option<String>,
}

#[tool_router]
impl RtkMcpServer {
    #[tool(
        name = "run_command",
        description = "Execute a shell command through RTK for token-optimized output. \
            Supports git, cargo, go, docker, nmap, nuclei, httpx, subfinder, ffuf, sqlmap, \
            curl, grep, find, jq, and 60+ other commands. \
            Output is filtered by RTK to reduce token consumption 60-90% while preserving \
            all essential info (errors, findings, summaries). \
            Falls back to raw execution if RTK unavailable. \
            Timeout: 120s. Max output: 64KB."
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

        let parts = shlex::split(&command)
            .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;

        if parts.is_empty() {
            return Err("Error: empty command after parsing".to_string());
        }

        let base_cmd = parts[0].rsplit('/').next().unwrap_or(&parts[0]);

        if !self.allow_all && !ALLOWED_COMMANDS.contains(&base_cmd) {
            return Err(format!(
                "Command '{}' is not in the allowlist. Set RTK_MCP_ALLOW_ALL=1 to bypass, \
                or use one of: {}",
                base_cmd,
                ALLOWED_COMMANDS.join(", ")
            ));
        }

        let parts_ref: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();

        // Shell operators (|, &&, $VAR, etc.) require sh -c — RTK can't handle them directly.
        // In that case skip RTK wrapping and go straight to sh -c execution.
        let has_shell_ops = needs_shell(&command);

        let result = if self.rtk_available && !has_shell_ops {
            match run_with_timeout("rtk", &parts_ref, cwd.as_deref()).await {
                Ok(out) => out,
                Err(rtk_err) => {
                    tracing::warn!("rtk failed ({}), falling back to raw command", rtk_err);
                    run_with_timeout(parts_ref[0], &parts_ref[1..], cwd.as_deref()).await?
                }
            }
        } else {
            // shell ops → run_with_timeout detects them and wraps in sh -c automatically
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
            Returns bytes saved, token reduction percentage, and command history."
    )]
    async fn get_rtk_savings(&self) -> Result<String, String> {
        if !self.rtk_available {
            return Ok("RTK not installed — no savings data available.\nInstall: cargo install --git https://github.com/rtk-ai/rtk".to_string());
        }
        run_with_timeout("rtk", &["gain"], None)
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
             nmap, nuclei, httpx, subfinder, ffuf, sqlmap, curl, grep, find, jq, and 60+ more. \
             Use the cwd parameter for directory control. Timeout is 120s.",
        )
    }
}

struct CommandResult {
    output: String,
    exit_code: i32,
    success: bool,
}

/// Returns true if the command string contains shell operators that require sh -c.
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

/// Execute a command asynchronously with timeout and output truncation.
/// If `full_cmd` is non-empty (shell mode), ignores cmd/args and runs via sh -c.
async fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: Option<&str>,
) -> Result<CommandResult, String> {
    // Reconstruct full command string to check for shell operators
    let full_cmd = if args.is_empty() {
        cmd.to_string()
    } else {
        format!("{} {}", cmd, args.join(" "))
    };

    let mut command = if needs_shell(&full_cmd) {
        let mut c = Command::new("sh");
        c.args(["-c", &full_cmd]);
        c
    } else {
        let mut c = Command::new(cmd);
        c.args(args);
        c
    };

    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = timeout(
        Duration::from_secs(COMMAND_TIMEOUT_SECS),
        command.output(),
    )
    .await
    .map_err(|_| format!("command timed out after {}s", COMMAND_TIMEOUT_SECS))?
    .map_err(|e| format!("failed to execute '{}': {}", cmd, e))?;

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

fn validate_rtk_installation() -> bool {
    std::process::Command::new("rtk")
        .arg("--version")
        .output()
        .map(|o| {
            let v = String::from_utf8_lossy(&o.stdout);
            v.starts_with("rtk ") && o.status.success()
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
