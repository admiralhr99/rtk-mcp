mod shell;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
    RoleServer, ServerHandler, ServiceExt,
};
use shell::Shell;
use tracing_subscriber::EnvFilter;

/// Maximum command string length (prevents memory abuse).
const MAX_COMMAND_LEN: usize = 4096;

/// Default output cap — override with RTK_MCP_MAX_OUTPUT_KB env var.
const DEFAULT_MAX_OUTPUT_KB: usize = 256;

/// Default timeout — override with RTK_MCP_TIMEOUT_SECS env var.
const DEFAULT_TIMEOUT_SECS: u64 = 300; // 5 min (covers cargo build, npm install, long scans)

/// How long `rtk rewrite` may take before the command is simply run unrewritten.
const REWRITE_TIMEOUT_SECS: u64 = 10;

/// How long to keep collecting output once the command itself has exited. Normally the output
/// pipes are closed by then; a background job that inherited them is not waited for (see
/// [`exec_with`]).
const BACKGROUND_LINGER_MS: u64 = 2000;

/// How long the readers get to see EOF after a timed-out command's process tree was killed.
const KILL_GRACE_MS: u64 = 500;

/// Appended to the output of a command that left a background job holding its output pipes.
const BACKGROUND_NOTE: &str = "\n[a background process is still holding this command's output \
    open — what it prints from now on is discarded; redirect it to a file \
    (`cmd > out.log 2>&1 &`) to keep it]";

/// Command execution timeout — overridden by RTK_MCP_TIMEOUT_SECS env var (default 300s).
fn command_timeout_secs() -> u64 {
    parse_timeout_secs(std::env::var("RTK_MCP_TIMEOUT_SECS").ok().as_deref())
}

/// A positive number of seconds; anything else (unset, junk, 0) is the default.
fn parse_timeout_secs(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse().ok())
        .filter(|&secs| secs > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
}

/// Maximum output size returned to the LLM (prevents context flooding) — overridden by
/// RTK_MCP_MAX_OUTPUT_KB env var (default 256KB).
fn max_output_bytes() -> usize {
    parse_max_output_bytes(std::env::var("RTK_MCP_MAX_OUTPUT_KB").ok().as_deref())
}

/// A positive number of kilobytes; anything else (unset, junk, 0, overflow) is the default.
fn parse_max_output_bytes(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&kb| kb > 0)
        .and_then(|kb| kb.checked_mul(1024))
        .unwrap_or(DEFAULT_MAX_OUTPUT_KB * 1024)
}

/// Per-command limits. Real calls build them from the environment; the tests build their own so
/// they never have to touch the (process-wide) environment.
#[derive(Clone, Copy)]
struct Limits {
    /// Wall-clock budget for the command itself.
    timeout_secs: u64,
    /// Bytes of output handed back to the LLM.
    max_output: usize,
    /// How long to wait for the output pipes to close after the command has exited.
    linger: Duration,
}

impl Limits {
    fn from_env() -> Self {
        Self {
            timeout_secs: command_timeout_secs(),
            max_output: max_output_bytes(),
            linger: Duration::from_millis(BACKGROUND_LINGER_MS),
        }
    }

    /// Bytes kept per stream while a command runs. Anything beyond is drained and dropped, so a
    /// runaway command (`yes`, `cat /dev/urandom | base64`) cannot exhaust the server's memory;
    /// the headroom over `max_output` is what lets an over-long stream be recognised as such.
    fn capture_bytes(&self) -> usize {
        self.max_output.saturating_mul(4)
    }
}

/// Safety-net allowlist used only when RTK is NOT available AND allow_all is false.
/// When RTK is present, rtk rewrite is used instead — RTK decides what it can filter.
#[rustfmt::skip] // keep the hand-grouped layout
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
    // Shells, archives and misc — carried over from the earlier local build's allowlist
    "bash", "sh", "zsh", "cut", "ps", "type", "date", "whoami", "id",
    "tar", "unzip", "zip",
];

/// Also allowed on Windows (and only there): the shells and built-ins a Windows session reaches for.
const WINDOWS_COMMANDS: &[&str] = &[
    "cmd",
    "powershell",
    "pwsh",
    "dir",
    "findstr",
    "where",
    "ipconfig",
    "tasklist",
    "hostname",
    "systeminfo",
];

/// Is `base` (a command name from [`shell::base_command`]) allowed while RTK is absent?
fn is_allowed_command(base: &str, windows: bool) -> bool {
    ALLOWED_COMMANDS.contains(&base) || (windows && WINDOWS_COMMANDS.contains(&base))
}

#[derive(Debug, Clone)]
pub struct RtkMcpServer {
    tool_router: ToolRouter<Self>,
    rtk_available: bool,
    /// `rtk --version` output (e.g. "rtk 0.49.0"), used in error hints for the stats tools.
    rtk_version: Option<String>,
    allow_all: bool,
}

impl RtkMcpServer {
    pub fn new() -> Self {
        let rtk_version = detect_rtk();
        let rtk_available = rtk_version.is_some();
        // When RTK is present it acts as the safety net, so allow_all defaults to true.
        // When RTK is absent the built-in allowlist is the only guard, so default to false.
        let allow_all = std::env::var("RTK_MCP_ALLOW_ALL")
            .map(|v| v == "1" || v.to_lowercase() == "true")
            .unwrap_or(rtk_available);

        if let Some(version) = &rtk_version {
            tracing::info!("{version} detected — commands will be routed through rtk rewrite");
        } else {
            tracing::warn!("RTK not found — commands will execute without filtering");
        }
        if allow_all && !rtk_available {
            tracing::warn!("RTK_MCP_ALLOW_ALL=1 — allowlist bypassed, all commands permitted");
        }
        // The shell is chosen, and logged, once at startup (on Windows this is where Git Bash is
        // looked for); the tool descriptions then say which one it is.
        let mut tool_router = Self::tool_router();
        describe_shell(&mut tool_router, Shell::current());

        Self {
            tool_router,
            rtk_available,
            rtk_version,
            allow_all,
        }
    }

    /// Run an `rtk` subcommand directly (no shell). A non-zero exit becomes a tool error, with the
    /// installed version added when the failure is an option that RTK predates.
    async fn run_rtk(&self, args: &[String], cwd: Option<&Path>) -> Result<String, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_with_timeout("rtk", &args, cwd).await?;
        if result.success {
            return Ok(result.output);
        }
        let mut output = result.output;
        output.push_str(&format!("\n[exit code: {}]", result.exit_code));
        // clap's wording for an option or subcommand the installed RTK doesn't know yet
        let predates = output.contains("unexpected argument") || output.contains("unrecognized");
        if let (true, Some(version)) = (predates, &self.rtk_version) {
            output.push_str(&format!(
                "\n[installed: {version} — it predates this option; upgrade RTK to use it]"
            ));
        }
        Err(output)
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
        description = "The command to execute — any command runs, and RTK compresses the output of the ones it has a filter for \
        (for example git, cargo, go, docker, kubectl, aws, gh, curl, grep, find, ls, jq, pytest, \
        ruff, mypy, tsc, vitest, rspec, rubocop). Security scanners such as nmap, nuclei, httpx, \
        subfinder, ffuf and sqlmap have no RTK filter and print everything. \
        Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported; runs under `sh -c` \
        (POSIX sh, not bash) — wrap bash-only syntax in `bash -c '...'`."
    )]
    command: String,

    #[schemars(
        description = "Working directory for the command. Must exist; must be inside \
        RTK_MCP_CWD_JAIL when the server is configured with one. Defaults to server cwd if omitted."
    )]
    cwd: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetRtkSavingsRequest {
    #[schemars(
        description = "Show ASCII bar chart of savings per command. Pass true for --graph."
    )]
    graph: Option<bool>,

    #[schemars(
        description = "Breakdown to show: summary (default), history (recent commands), daily, \
        weekly, monthly, all (daily+weekly+monthly), failures (commands that fell back to raw \
        execution), recalls (recall efficiency per filter — needs RTK 0.49+) or quota \
        (monthly subscription-quota estimate)."
    )]
    view: Option<SavingsView>,

    #[schemars(
        description = "Only count commands run in the project directory (--project). See cwd."
    )]
    project: Option<bool>,

    #[schemars(description = "Output format: text (default), json or csv.")]
    format: Option<StatsFormat>,

    #[schemars(description = "Subscription tier for view=quota: pro, 5x or 20x (default 20x).")]
    tier: Option<QuotaTier>,

    #[schemars(
        description = "Directory treated as the current project when project=true. \
        Defaults to server cwd."
    )]
    cwd: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct GetRtkDiscoverRequest {
    #[schemars(
        description = "Scan all projects instead of only the current one (--all). \
        The server's cwd is rarely your project, so this or `project` is usually what you want."
    )]
    all: Option<bool>,

    #[schemars(description = "Only include sessions from the last N days (default 30).")]
    since: Option<u32>,

    #[schemars(description = "Filter by project path (substring match).")]
    project: Option<String>,

    #[schemars(description = "Max commands per section (default 15).")]
    limit: Option<u32>,

    #[schemars(description = "Output format: text (default) or json.")]
    format: Option<DiscoverFormat>,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum SavingsView {
    Summary,
    History,
    Daily,
    Weekly,
    Monthly,
    All,
    Failures,
    Recalls,
    Quota,
}

impl SavingsView {
    /// The `rtk gain` flag for this view (`summary` is the flag-less default).
    fn flag(self) -> Option<&'static str> {
        match self {
            SavingsView::Summary => None,
            SavingsView::History => Some("--history"),
            SavingsView::Daily => Some("--daily"),
            SavingsView::Weekly => Some("--weekly"),
            SavingsView::Monthly => Some("--monthly"),
            SavingsView::All => Some("--all"),
            SavingsView::Failures => Some("--failures"),
            SavingsView::Recalls => Some("--recalls"),
            SavingsView::Quota => Some("--quota"),
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StatsFormat {
    Text,
    Json,
    Csv,
}

impl StatsFormat {
    fn as_str(self) -> &'static str {
        match self {
            StatsFormat::Text => "text",
            StatsFormat::Json => "json",
            StatsFormat::Csv => "csv",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum DiscoverFormat {
    Text,
    Json,
}

impl DiscoverFormat {
    fn as_str(self) -> &'static str {
        match self {
            DiscoverFormat::Text => "text",
            DiscoverFormat::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Deserialize, schemars::JsonSchema)]
enum QuotaTier {
    #[serde(rename = "pro")]
    Pro,
    #[serde(rename = "5x")]
    X5,
    #[serde(rename = "20x")]
    X20,
}

impl QuotaTier {
    fn as_str(self) -> &'static str {
        match self {
            QuotaTier::Pro => "pro",
            QuotaTier::X5 => "5x",
            QuotaTier::X20 => "20x",
        }
    }
}

/// Build the `rtk gain` argv. Values go in as `--opt=value` so nothing a client sends can be
/// mistaken for a separate option. `--reset` is deliberately not reachable from here.
fn gain_args(req: &GetRtkSavingsRequest) -> Vec<String> {
    let mut args = vec!["gain".to_string()];
    if req.graph.unwrap_or(false) {
        args.push("--graph".to_string());
    }
    if let Some(flag) = req.view.and_then(SavingsView::flag) {
        args.push(flag.to_string());
    }
    if req.project.unwrap_or(false) {
        args.push("--project".to_string());
    }
    if let Some(format) = req.format {
        args.push(format!("--format={}", format.as_str()));
    }
    if let (Some(SavingsView::Quota), Some(tier)) = (req.view, req.tier) {
        args.push(format!("--tier={}", tier.as_str()));
    }
    args
}

/// Build the `rtk discover` argv (same `--opt=value` convention as `gain_args`).
fn discover_args(req: &GetRtkDiscoverRequest) -> Vec<String> {
    let mut args = vec!["discover".to_string()];
    if req.all.unwrap_or(false) {
        args.push("--all".to_string());
    }
    if let Some(days) = req.since {
        args.push(format!("--since={days}"));
    }
    if let Some(project) = &req.project {
        args.push(format!("--project={project}"));
    }
    if let Some(limit) = req.limit {
        args.push(format!("--limit={limit}"));
    }
    if let Some(format) = req.format {
        args.push(format!("--format={}", format.as_str()));
    }
    args
}

#[tool_router]
impl RtkMcpServer {
    #[tool(
        name = "run_command",
        description = "Execute ANY shell command through RTK for token-optimized output. \
            Uses `rtk rewrite` to route commands through the appropriate RTK filter (60-90% \
            token savings). Commands without an RTK filter execute directly with full output. \
            RTK filters, among others, git, cargo, go, docker, kubectl, aws, gh, curl, grep, \
            find, ls, jq, pytest, ruff, mypy, tsc, vitest, rspec and rubocop (the exact set \
            depends on the RTK version). It has no filter for security scanners such as nmap, \
            nuclei, httpx, subfinder, ffuf or sqlmap, so cap their output yourself (head, grep, \
            an output file). \
            Shell operators (|, &&, ||, ;, >, <, $VAR, *, ~, backticks) are fully supported; the \
            command runs under `sh -c` (POSIX sh, not bash), so wrap bash-only syntax ({1..5}, \
            [[ ]], <(...), &>) in `bash -c '...'`. \
            Falls back to raw execution if RTK unavailable or has no filter for the command. \
            Timeout: configurable via RTK_MCP_TIMEOUT_SECS (default 300s); on timeout the whole \
            process tree is killed and the output printed so far is returned with the error. \
            Max output: 256KB, configurable via RTK_MCP_MAX_OUTPUT_KB. Commands matching a deny \
            rule in the user's Claude settings are refused. Background jobs (`cmd &`, `ssh -f`) \
            survive the call, but only if their output goes to a file (`> log 2>&1 &`). \
            Prefix a command with RTK_DISABLED=1 for raw, \
            unfiltered output; if RTK elides output it prints a hint (`rtk recall <id>` or a \
            saved-file path) for retrieving the full text.",
        annotations(
            title = "Run command (RTK-filtered)",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run_command(
        &self,
        Parameters(RunCommandRequest { command, cwd }): Parameters<RunCommandRequest>,
        context: RequestContext<RoleServer>,
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

        // The working directory must exist and, when RTK_MCP_CWD_JAIL is set, lie inside it.
        let cwd = cwd.as_deref().map(validate_cwd).transpose()?;

        // Enforce allowlist only when RTK is absent AND allow_all is false.
        // When RTK is available, `rtk rewrite` decides what it can filter;
        // unknown commands fall back to direct execution anyway.
        if !self.allow_all && !self.rtk_available {
            let program = shell::program_of(&command, cfg!(windows))?;
            let base_cmd = shell::base_command(&program, cfg!(windows));
            if !is_allowed_command(&base_cmd, cfg!(windows)) {
                return Err(format!(
                    "Command '{}' is not in the allowlist. Set RTK_MCP_ALLOW_ALL=1 to bypass, \
                    or use one of: {}",
                    base_cmd,
                    ALLOWED_COMMANDS.join(", ")
                ));
            }
        }

        // Route through RTK using `rtk rewrite` — the canonical hook logic.
        // rewrite returns the token-optimized form (e.g. "cargo test | rtk grep FAILED")
        // or nothing if RTK has no filter for the command. It also enforces the deny rules
        // from the user's Claude settings (exit 2) — a denied command is never executed.
        let effective_command = if self.rtk_available {
            match get_rtk_rewrite(&command, cwd.as_deref()).await {
                Rewrite::Rewritten(rewritten) => rewritten,
                Rewrite::Passthrough => command.clone(),
                Rewrite::Denied => {
                    return Err(format!(
                        "Denied: a deny rule in your Claude settings (permissions.deny) matches \
                        this command, so it was not executed: {}",
                        command
                    ));
                }
            }
        } else {
            command.clone()
        };

        // A client can cancel the call (`notifications/cancelled`). rmcp does not drop the handler
        // when that happens — it only fires this token — so race the command against it: dropping
        // the exec future is what makes `GroupKill` SIGKILL the command's whole process tree.
        // `biased`: a call cancelled while the rewrite probe ran must not start the command at all.
        let result = tokio::select! {
            biased;
            _ = context.ct.cancelled() => return Err("cancelled by the client".to_string()),
            result = run_shell_cmd(&effective_command, cwd.as_deref()) => result?,
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
        description = "Show RTK token savings statistics. \
            Returns bytes saved, token reduction percentage, and command history. \
            Pass graph=true for an ASCII bar chart; view selects a breakdown (history, daily, \
            weekly, monthly, all, failures, recalls, quota); format=json for machine-readable \
            output.",
        annotations(
            title = "RTK token savings",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_rtk_savings(
        &self,
        Parameters(request): Parameters<GetRtkSavingsRequest>,
    ) -> Result<String, String> {
        if !self.rtk_available {
            return Ok(format!(
                "RTK not installed — no savings data available.\nInstall: {}",
                install_hint()
            ));
        }
        let cwd = request.cwd.as_deref().map(validate_cwd).transpose()?;
        self.run_rtk(&gain_args(&request), cwd.as_deref()).await
    }

    #[tool(
        name = "get_rtk_discover",
        description = "Analyze Claude Code command history to discover missed RTK optimization \
            opportunities. Shows which commands could have been routed through RTK for better \
            token savings. Pass all=true (or project=<path substring>) — by default only the \
            server's own working directory is scanned.",
        annotations(
            title = "RTK missed-savings discovery",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_rtk_discover(
        &self,
        Parameters(request): Parameters<GetRtkDiscoverRequest>,
    ) -> Result<String, String> {
        if !self.rtk_available {
            return Ok(format!(
                "RTK not installed — discovery unavailable.\nInstall: {}",
                install_hint()
            ));
        }
        self.run_rtk(&discover_args(&request), None).await
    }

    #[tool(
        name = "get_rtk_session",
        description = "Show RTK session statistics and active session information.",
        annotations(
            title = "RTK session adoption",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn get_rtk_session(&self) -> Result<String, String> {
        if !self.rtk_available {
            return Ok(format!(
                "RTK not installed — session data unavailable.\nInstall: {}",
                install_hint()
            ));
        }
        self.run_rtk(&["session".to_string()], None).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RtkMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rtk-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions(Shell::current()))
    }
}

/// The sentence about the shell in the `command` parameter's description (see
/// [`RunCommandRequest`]), kept here so it can be swapped out when the shell is not `sh`.
const PARAM_SHELL_CLAUSE: &str =
    "runs under `sh -c` (POSIX sh, not bash) — wrap bash-only syntax in `bash -c '...'`.";

/// The same sentence in the description of the `run_command` tool.
const TOOL_SHELL_CLAUSE: &str = "the command runs under `sh -c` (POSIX sh, not bash), so wrap \
     bash-only syntax ({1..5}, [[ ]], <(...), &>) in `bash -c '...'`.";

/// The descriptions in the source describe `sh -c`: right on Linux and macOS, where they are left
/// exactly as written. On Windows the shell is Git Bash or cmd.exe, so `run_command` says so.
fn describe_shell(router: &mut ToolRouter<RtkMcpServer>, shell: &Shell) {
    let Some(clause) = shell.runs_under() else {
        return;
    };
    let Some(route) = router.map.get_mut("run_command") else {
        return;
    };
    let tool = &mut route.attr;
    if let Some(text) = tool.description.take() {
        let adapted = text.replace(TOOL_SHELL_CLAUSE, &format!("the command {clause}"));
        tool.description = Some(adapted.into());
    }
    let mut schema = (*tool.input_schema).clone();
    if let Some(serde_json::Value::String(text)) = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("command"))
        .and_then(|command| command.get_mut("description"))
    {
        *text = text.replace(PARAM_SHELL_CLAUSE, &clause);
    }
    tool.input_schema = Arc::new(schema);
}

/// How to install RTK, for the stats tools when it is missing.
fn install_hint() -> &'static str {
    if cfg!(windows) {
        "winget install rtk-ai.rtk"
    } else {
        "cargo install --git https://github.com/rtk-ai/rtk"
    }
}

/// Windows: keep a console window from flashing up for a probe.
fn hide_console(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(shell::CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// The server instructions up to the sentence about shell syntax.
const INSTRUCTIONS_HEAD: &str =
    "IMPORTANT: Use run_command for ALL shell execution instead of any built-in bash tool. \
     RTK filters output to reduce token usage 60-90% for the commands it knows (git, \
     cargo, go, docker, kubectl, aws, gh, curl, grep, find, ls, jq, pytest, ruff, mypy, \
     tsc, vitest, rspec, rubocop and more, depending on the RTK version). Any other \
     command runs as-is with full output: security scanners such as nmap, nuclei, \
     httpx, subfinder, ffuf and sqlmap are not filtered, so cap their output yourself. ";

/// The server instructions after the sentence about shell syntax.
const INSTRUCTIONS_TAIL: &str =
    "Always pass 'cwd' so commands run in the right directory. Timeout is configurable \
     via RTK_MCP_TIMEOUT_SECS (default 300s), the output cap via RTK_MCP_MAX_OUTPUT_KB \
     (default 256KB). A background job (`cmd &`, `ssh -f`) must send its output to a \
     file (`> log 2>&1 &`) or it is cut loose from the call after a few seconds. \
     If RTK elides part of an output it prints a recall hint (`rtk recall <id>` or a \
     saved-file path) — run it with run_command to get the full text. \
     Prefix a command with RTK_DISABLED=1 to bypass filtering and get raw output.";

/// What the server tells the model when it connects; `shell` supplies the sentence about syntax.
fn instructions(shell: &Shell) -> String {
    format!(
        "{INSTRUCTIONS_HEAD}{} {INSTRUCTIONS_TAIL}",
        shell.syntax_note()
    )
}

struct CommandResult {
    output: String,
    exit_code: i32,
    success: bool,
}

/// Validate that cwd exists and, if RTK_MCP_CWD_JAIL is set, lies within that root.
///
/// Both sides are canonicalized, so `..` segments and symlinks cannot be used to step outside.
/// This guards the `cwd` parameter only — it is not a sandbox: the command itself can still
/// `cd` elsewhere or name absolute paths.
fn validate_cwd(cwd: &str) -> Result<PathBuf, String> {
    let abs = std::fs::canonicalize(cwd).map_err(|e| format!("invalid cwd '{}': {}", cwd, e))?;
    if let Ok(jail) = std::env::var("RTK_MCP_CWD_JAIL") {
        let root = std::fs::canonicalize(&jail)
            .map_err(|e| format!("invalid jail root '{}': {}", jail, e))?;
        if !abs.starts_with(&root) {
            return Err(format!("cwd '{}' is outside allowed root '{}'", cwd, jail));
        }
    }
    Ok(shell::plain_path(abs))
}

/// What `rtk rewrite` decided about a command.
#[derive(Debug, PartialEq, Eq)]
enum Rewrite {
    /// RTK has a token-optimized form (e.g. "cargo test | rtk grep FAILED").
    Rewritten(String),
    /// RTK has no filter for the command — it runs exactly as given.
    Passthrough,
    /// A deny rule in the user's Claude settings matches — the command must not run.
    Denied,
}

/// Map `rtk rewrite`'s exit code and stdout onto a [`Rewrite`].
///
/// # The exit-code protocol
/// * `0` rewritten and allowed by an allow rule (RTK < 0.49 uses this for every rewrite)
/// * `1` no RTK equivalent — run the command as-is
/// * `2` a deny rule matched — stdout is empty and the command must not run
/// * `3` rewritten, no allow rule, i.e. the "ask"/default case (RTK 0.49+ uses this for every
///   rewrite that has no allow rule)
///
/// A tool call cannot ask the user a follow-up question — the MCP client's own approval prompt
/// for `run_command` is that gate — so `3` is run like `0`. Only `0`/`3` count as a rewrite: the
/// exit code is checked as well as stdout, so an unexpected failure that happens to print
/// something can never end up being executed via `sh -c`.
fn interpret_rewrite(exit_code: Option<i32>, stdout: &str) -> Rewrite {
    match exit_code {
        Some(2) => Rewrite::Denied,
        Some(0) | Some(3) => match stdout.trim() {
            "" => Rewrite::Passthrough,
            rewritten => Rewrite::Rewritten(rewritten.to_string()),
        },
        _ => Rewrite::Passthrough,
    }
}

/// Ask RTK for the token-optimized equivalent of a command string.
///
/// Uses `rtk rewrite` — the single source of truth for RTK's hook logic (command mapping and
/// the user's deny rules). See [`interpret_rewrite`] for how the answer is read.
///
/// `cwd` matters: RTK finds project-level Claude settings (`.claude/settings.json`, where deny
/// rules can live) by walking up from its working directory.
async fn get_rtk_rewrite(command: &str, cwd: Option<&Path>) -> Rewrite {
    rewrite_via("rtk", command, cwd).await
}

/// [`get_rtk_rewrite`] with an explicit RTK binary (lets the tests use a stub).
///
/// # Why stdout-only (not merged stdout+stderr)
/// `run_with_timeout` merges stdout and stderr into a single string.  If `rtk rewrite`
/// ever emits a warning or update notice to stderr, the merged string would look like
/// "rtk cargo test\n[stderr]\nWarning: …" — which would then be passed to `sh -c` and
/// fail with a syntax error.  We capture stdout separately here.
///
/// # Why `--`
/// Without it, a command that starts like an option is parsed by `rtk` itself:
/// `rtk rewrite --help` prints usage text and exits 0, which would then be run as a command.
///
/// A rewrite that cannot be obtained at all (spawn error, timeout) is `Passthrough`: the
/// command runs unrewritten, as it always did when RTK has nothing to say about it.
async fn rewrite_via(program: &str, command: &str, cwd: Option<&Path>) -> Rewrite {
    let mut cmd = Command::new(program);
    cmd.args(["rewrite", "--", command]);
    cmd.stdin(Stdio::null()); // isolate from MCP transport fd
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(shell::CREATE_NO_WINDOW);
    child_env(&mut cmd);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = match timeout(Duration::from_secs(REWRITE_TIMEOUT_SECS), cmd.output()).await {
        Ok(Ok(output)) => output,
        _ => return Rewrite::Passthrough, // timeout or I/O error
    };
    interpret_rewrite(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
    )
}

/// Environment tweaks for every child process.
///
/// RTK prints a "no hook installed" warning to stderr once a day. Here it would land in the
/// output the LLM reads, and this server *is* the integration, so silence it (unless the user
/// set the variable themselves).
fn child_env(command: &mut Command) {
    if std::env::var_os("RTK_SUPPRESS_HOOK_WARNING").is_none() {
        command.env("RTK_SUPPRESS_HOOK_WARNING", "1");
    }
    // Windows: Python encodes what it writes to a pipe in the ANSI code page, and fails with a
    // UnicodeEncodeError on the first character outside it. The output is read as UTF-8.
    #[cfg(windows)]
    if std::env::var_os("PYTHONIOENCODING").is_none()
        && !command.as_std().get_envs().any(|(key, _)| {
            key.to_str()
                .is_some_and(|k| k.eq_ignore_ascii_case("PYTHONIOENCODING"))
        })
    {
        command.env("PYTHONIOENCODING", "utf-8");
    }
    // Windows: an rtk.exe installed next to rtk-mcp.exe has to be found by the shell as well. The
    // PATH to extend is the one the command already carries (a POSIX shell's tools directory is put
    // there by `Shell::command`), else this process's own.
    if let Some(dir) = shell::bundled_rtk_dir() {
        let current = command
            .as_std()
            .get_envs()
            .find(|(key, _)| key.to_str().is_some_and(|k| k.eq_ignore_ascii_case("PATH")))
            .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
            .or_else(|| std::env::var_os("PATH"));
        command.env(
            "PATH",
            shell::path_with_dirs_first(current.as_deref(), &[dir]),
        );
    }
}

/// Execute a command string in the platform's shell: `sh -c` on Linux and macOS (supports shell
/// operators and env var expansion), Git Bash or cmd.exe on Windows (see [`Shell`]).
///
/// See [`exec`] for stdin isolation, process-group handling and the timeout.
async fn run_shell_cmd(shell_str: &str, cwd: Option<&Path>) -> Result<CommandResult, String> {
    run_in_shell(Shell::current(), shell_str, cwd).await
}

/// [`run_shell_cmd`] with an explicit shell (lets the tests pick one).
async fn run_in_shell(
    shell: &Shell,
    shell_str: &str,
    cwd: Option<&Path>,
) -> Result<CommandResult, String> {
    exec(
        Command::from(shell.command(shell_str)),
        cwd,
        "shell command",
    )
    .await
}

/// Execute a command directly (no shell expansion). Used for RTK probes and savings tools.
/// Same handling as `run_shell_cmd`.
async fn run_with_timeout(
    cmd: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<CommandResult, String> {
    let mut command = Command::new(cmd);
    command.args(args);
    exec(command, cwd, &format!("'{}'", cmd)).await
}

/// Run `command` with this server's process hygiene and collect its output.
///
/// # stdin isolation
/// Child processes **must never inherit the MCP server's stdin** (fd 0).  The rmcp
/// stdio transport sets fd 0 to O_NONBLOCK via tokio's event loop.  If a child
/// inherits that fd it can: read MCP protocol bytes, change the O_NONBLOCK flag, or
/// leave the fd in a state where the next tokio read returns EAGAIN (errno 35 on
/// macOS) — which crashes the transport with "Resource temporarily unavailable".
/// Setting `Stdio::null()` prevents all of this.
///
/// # Process group
/// The child leads its own process group. When the timeout fires — or the client cancels the
/// request and this future is dropped — the **whole group** is SIGKILLed, not just the direct
/// child: `kill_on_drop` alone only reaches `sh`, so a timed-out `nmap`/`ffuf` it started would
/// keep running unseen after the client was told the command had timed out. `kill_on_drop` stays
/// as the backstop for the direct child.
///
/// # Output
/// Each stream keeps at most `Limits::capture_bytes`; the rest is drained and dropped.
///
/// # Background jobs
/// The call returns when the *command* exits, not when its output pipes close. A job the command
/// backgrounded and left attached to those pipes (`ssh -fN …`, `nohup server &`, `sleep 30 &`)
/// would otherwise hold the call until the timeout, which then kills the very job the command
/// meant to leave running. So once the command has exited its output gets `Limits::linger` to
/// arrive, the call returns, and the readers keep draining in the background: the job must not
/// block on a full pipe or die of SIGPIPE just because this call returned. The reply says so.
async fn exec(command: Command, cwd: Option<&Path>, what: &str) -> Result<CommandResult, String> {
    exec_with(command, cwd, what, Limits::from_env()).await
}

/// [`exec`] with explicit limits (lets the tests use short ones without touching the env).
async fn exec_with(
    mut command: Command,
    cwd: Option<&Path>,
    what: &str,
    limits: Limits,
) -> Result<CommandResult, String> {
    command.stdin(Stdio::null()); // ← isolate from MCP stdio transport
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true); // ← backstop: kill the direct child if this future is dropped
    #[cfg(unix)]
    command.process_group(0); // ← child leads its own group so the whole tree can be killed
    #[cfg(windows)]
    command.creation_flags(shell::CREATE_NO_WINDOW); // ← no console window per command
    child_env(&mut command);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to execute {}: {}", what, e))?;
    let mut group = GroupKill::new(child.id());
    let mut streams = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => Streams::start(stdout, stderr, limits.capture_bytes()),
        _ => return Err(format!("failed to capture output of {}", what)),
    };

    let status = match timeout(Duration::from_secs(limits.timeout_secs), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(format!("failed to execute {}: {}", what, e)),
        Err(_) => {
            // Timed out: SIGKILL the whole process tree, then give the readers a moment to reach
            // EOF so that what the command printed before the cutoff — half a scan is still
            // worth having — comes back with the error.
            drop(group);
            streams
                .closed_within(Duration::from_millis(KILL_GRACE_MS))
                .await;
            let (stdout, stderr) = streams.bytes();
            streams.abort(); // whatever escaped the kill no longer gets to hold our pipes
            let mut message = format!("command timed out after {}s", limits.timeout_secs);
            if !(stdout.is_empty() && stderr.is_empty()) {
                message.push_str("\n[output before the timeout]\n");
                message.push_str(&render_output(&stdout, &stderr, limits.max_output));
            }
            return Err(message);
        }
    };

    // It exited on its own: leave alone anything it deliberately backgrounded.
    group.disarm();

    let closed = streams.closed_within(limits.linger).await;
    let (stdout, stderr) = streams.bytes();
    let mut result = finish_output(
        std::process::Output {
            status,
            stdout,
            stderr,
        },
        limits.max_output,
    );
    if !closed {
        // The readers stay behind, detached, and keep draining until the job lets go of the pipes.
        result.output.push_str(BACKGROUND_NOTE);
    }
    Ok(result)
}

/// Kills a command's whole process tree when dropped, unless disarmed: SIGKILL to its process
/// group on Unix, a job object on Windows (`taskkill /T` if the process could not be put in one).
#[cfg_attr(not(any(unix, windows)), allow(dead_code))]
struct GroupKill {
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<shell::Job>,
}

impl GroupKill {
    fn new(pid: Option<u32>) -> Self {
        Self {
            pid,
            // Straight after the spawn: a shell needs far longer than this to start its own
            // children, so they are born into the job.
            #[cfg(windows)]
            job: pid.and_then(shell::Job::holding),
        }
    }

    fn disarm(&mut self) {
        self.pid = None;
        #[cfg(windows)]
        {
            self.job = None; // closing the handle leaves what is in the job running
        }
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid.filter(|&pid| pid > 1) {
            // SAFETY: a plain syscall. The child was spawned with process_group(0), so `pid` is
            // its group id and a negative pid addresses that whole group. `pid > 1` makes sure
            // we can never signal "every process" (-1) or our own group (0).
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        if let Some(pid) = self.pid {
            match &self.job {
                Some(job) => job.kill(),
                None => shell::kill_tree(pid),
            }
        }
    }
}

/// One output stream of a running command, filled in by a task that reads the pipe to EOF.
///
/// A task rather than a future polled alongside the process, so that it can outlive the call:
/// see "Background jobs" on [`exec`].
struct Capture {
    kept: Arc<Mutex<Vec<u8>>>,
    task: JoinHandle<()>,
}

impl Capture {
    /// Keep at most `cap` bytes of `reader`; the rest is read and dropped so the writer never
    /// blocks on a full pipe.
    fn start<R: AsyncRead + Unpin + Send + 'static>(mut reader: R, cap: usize) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&kept);
        let task = tokio::spawn(async move {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut kept = shared.lock().unwrap_or_else(PoisonError::into_inner);
                        let room = cap.saturating_sub(kept.len());
                        kept.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        });
        Self { kept, task }
    }

    fn bytes(&self) -> Vec<u8> {
        self.kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The stdout and stderr [`Capture`]s of one command.
struct Streams {
    out: Capture,
    err: Capture,
}

impl Streams {
    fn start<O, E>(stdout: O, stderr: E, cap: usize) -> Self
    where
        O: AsyncRead + Unpin + Send + 'static,
        E: AsyncRead + Unpin + Send + 'static,
    {
        Self {
            out: Capture::start(stdout, cap),
            err: Capture::start(stderr, cap),
        }
    }

    /// Wait up to `grace` for both streams to reach EOF; true if they did.
    async fn closed_within(&mut self, grace: Duration) -> bool {
        let _ = timeout(grace, async {
            let _ = (&mut self.out.task).await;
            let _ = (&mut self.err.task).await;
        })
        .await;
        self.out.task.is_finished() && self.err.task.is_finished()
    }

    /// What has been read so far, as (stdout, stderr).
    fn bytes(&self) -> (Vec<u8>, Vec<u8>) {
        (self.out.bytes(), self.err.bytes())
    }

    /// Stop reading: the read ends of the pipes close.
    fn abort(&self) {
        self.out.task.abort();
        self.err.task.abort();
    }
}

/// Combine the two streams and cut the result down to `max_output` bytes.
fn render_output(stdout: &[u8], stderr: &[u8], max_output: usize) -> String {
    let mut combined = collect_output(stdout, stderr);
    if combined.len() > max_output {
        truncate_utf8(&mut combined, max_output);
        combined.push_str(&format!(
            "\n[output truncated at {}KB — use head/tail/grep to filter]",
            max_output / 1024
        ));
    }
    combined
}

fn finish_output(output: std::process::Output, max_output: usize) -> CommandResult {
    CommandResult {
        output: render_output(&output.stdout, &output.stderr, max_output),
        exit_code: output.status.code().unwrap_or(-1),
        success: output.status.success(),
    }
}

/// Truncate to at most `max_bytes`, backing up to a char boundary.
///
/// `String::truncate` panics when the cut lands inside a multi-byte character — and with
/// `panic = "abort"` that takes the whole server down. Any non-ASCII output over the limit
/// (a page of CJK text, a filename list with accents) would trigger it.
fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

fn collect_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(no output)".to_string(),
        (false, true) => stdout.into_owned(),
        (true, false) => stderr.into_owned(),
        (false, false) => format!("{}\n[stderr]\n{}", stdout, stderr),
    }
}

/// Validate RTK installation by checking --version and the rewrite subcommand.
/// Stdin is explicitly set to null so these probes never touch the MCP transport fd.
/// Returns the `rtk --version` line (e.g. "rtk 0.49.0") when RTK is usable.
fn detect_rtk() -> Option<String> {
    let mut version_probe = std::process::Command::new("rtk");
    version_probe.arg("--version").stdin(Stdio::null());
    hide_console(&mut version_probe);
    let version_out = version_probe.output().ok()?;
    let version = String::from_utf8_lossy(&version_out.stdout)
        .trim()
        .to_string();
    if !(version.starts_with("rtk ") && version_out.status.success()) {
        return None;
    }

    // Verify `rtk rewrite` works — that's the core routing mechanism this server relies on.
    let mut probe = std::process::Command::new("rtk");
    probe
        .args(["rewrite", "--", "git status"])
        .stdin(Stdio::null());
    if std::env::var_os("RTK_SUPPRESS_HOOK_WARNING").is_none() {
        probe.env("RTK_SUPPRESS_HOOK_WARNING", "1");
    }
    hide_console(&mut probe);
    let output = probe.output().ok()?;
    match interpret_rewrite(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
    ) {
        Rewrite::Passthrough => None,
        // A deny rule for `git status` still proves that `rtk rewrite` itself works.
        Rewrite::Rewritten(_) | Rewrite::Denied => Some(version),
    }
}

fn main() -> anyhow::Result<()> {
    // Windows: this executable is also the launcher of cmd.exe commands (`shell::run_cmd_launcher`),
    // which has to run before anything of the server starts.
    #[cfg(windows)]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if args
            .get(1)
            .is_some_and(|arg| arg == shell::CMD_LAUNCHER_ARG)
        {
            let code = match (args.get(2), args.get(3)) {
                (Some(cmd), Some(line)) => shell::run_cmd_launcher(cmd, line),
                _ => {
                    eprintln!(
                        "usage: rtk-mcp {} <cmd.exe> <command line>",
                        shell::CMD_LAUNCHER_ARG
                    );
                    2
                }
            };
            std::process::exit(code);
        }
        if let Ok(exe) = std::env::current_exe() {
            shell::use_cmd_launcher(exe);
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> anyhow::Result<()> {
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

    // Serializes all tests that mutate RTK_MCP_TIMEOUT_SECS / RTK_MCP_CWD_JAIL (or any other
    // process-wide environment variable) to prevent races.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Explicit limits for tests that run a command: nothing here reads the environment, so they
    /// cannot race with the tests that change it. The short linger keeps the background-job
    /// tests quick.
    fn limits(timeout_secs: u64) -> Limits {
        Limits {
            timeout_secs,
            max_output: DEFAULT_MAX_OUTPUT_KB * 1024,
            linger: Duration::from_millis(300),
        }
    }

    #[test]
    fn command_timeout_secs_default() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
        assert_eq!(command_timeout_secs(), 300);
    }

    #[test]
    fn command_timeout_secs_from_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::set_var("RTK_MCP_TIMEOUT_SECS", "45");
        assert_eq!(command_timeout_secs(), 45);
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
    }

    #[test]
    fn command_timeout_secs_invalid_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::set_var("RTK_MCP_TIMEOUT_SECS", "notanumber");
        assert_eq!(command_timeout_secs(), 300);
        std::env::remove_var("RTK_MCP_TIMEOUT_SECS");
    }

    /// The defaults are a compile-time floor: they cover long builds and scans, and big outputs.
    #[test]
    fn defaults_are_generous() {
        const {
            assert!(
                DEFAULT_TIMEOUT_SECS >= 300,
                "timeout must cover long builds"
            );
            assert!(DEFAULT_MAX_OUTPUT_KB >= 256, "output cap too small");
        }
    }

    #[test]
    fn parse_timeout_secs_accepts_only_positive_numbers() {
        assert_eq!(parse_timeout_secs(None), 300);
        assert_eq!(parse_timeout_secs(Some("45")), 45);
        assert_eq!(parse_timeout_secs(Some(" 600 ")), 600);
        for bad in ["", "0", "-5", "1.5", "5m", "abc"] {
            assert_eq!(parse_timeout_secs(Some(bad)), 300, "{:?}", bad);
        }
    }

    #[test]
    fn parse_max_output_bytes_reads_kilobytes() {
        assert_eq!(parse_max_output_bytes(None), 256 * 1024);
        assert_eq!(parse_max_output_bytes(Some("512")), 512 * 1024);
        assert_eq!(parse_max_output_bytes(Some(" 1 ")), 1024);
        // 0 would truncate everything; junk and overflow are not sizes
        for bad in ["", "0", "-1", "1.5", "64KB", "abc", "18446744073709551615"] {
            assert_eq!(parse_max_output_bytes(Some(bad)), 256 * 1024, "{:?}", bad);
        }
    }

    #[test]
    fn max_output_bytes_from_env() {
        let _g = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("RTK_MCP_MAX_OUTPUT_KB");
        assert_eq!(max_output_bytes(), 256 * 1024);
        std::env::set_var("RTK_MCP_MAX_OUTPUT_KB", "16");
        assert_eq!(max_output_bytes(), 16 * 1024);
        std::env::remove_var("RTK_MCP_MAX_OUTPUT_KB");
    }

    #[test]
    fn capture_headroom_scales_with_the_output_cap() {
        let small = Limits {
            max_output: 1024,
            ..limits(1)
        };
        assert_eq!(small.capture_bytes(), 4 * 1024);
        let huge = Limits {
            max_output: usize::MAX,
            ..limits(1)
        };
        assert_eq!(
            huge.capture_bytes(),
            usize::MAX,
            "must saturate, not overflow"
        );
    }

    #[test]
    fn collect_output_combines_streams() {
        assert_eq!(collect_output(b"hello world", b""), "hello world");
        assert_eq!(collect_output(b"", b"only err"), "only err");
        assert_eq!(collect_output(b"", b""), "(no output)");
        assert_eq!(collect_output(b"out", b"err"), "out\n[stderr]\nerr");
    }

    /// A command that runs `line` in a POSIX shell: `sh`, or Git Bash on Windows. `None` on a
    /// Windows machine without Git, where the tests that need one have nothing to run.
    fn posix(line: &str) -> Option<Command> {
        Shell::posix().map(|shell| Command::from(shell.command(line)))
    }

    #[tokio::test]
    async fn run_with_timeout_basic() {
        // a real program on every platform (`echo` is a cmd.exe built-in on Windows)
        let result = run_with_timeout("cargo", &["--version"], None)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("cargo"));
    }

    /// `echo` means the same in sh, Git Bash and cmd.exe, so this runs in whichever shell the
    /// platform uses.
    #[tokio::test]
    async fn run_shell_cmd_runs_in_the_platform_shell() {
        let result = run_shell_cmd("echo hello", None).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output.trim(), "hello");
    }

    #[tokio::test]
    async fn run_shell_cmd_pipe() {
        let Some(shell) = Shell::posix() else { return };
        let result = run_in_shell(&shell, "echo hello | tr a-z A-Z", None)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("HELLO"));
    }

    #[tokio::test]
    async fn run_shell_cmd_env_var() {
        let Some(shell) = Shell::posix() else { return };
        let result = run_in_shell(&shell, "echo $HOME", None).await.unwrap();
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

    /// The earlier local build allowed these too; the union of both lists is kept.
    #[test]
    fn allowlist_keeps_the_shells_and_extras_of_the_earlier_local_build() {
        for cmd in [
            "bash", "sh", "zsh", "jq", "awk", "sed", "diff", "sort", "uniq", "cut", "xargs", "ps",
            "ss", "ip", "ping", "netstat", "which", "type", "date", "whoami", "id", "tar", "unzip",
            "zip", "sqlite3", "python", "python3", "rtk",
        ] {
            assert!(
                ALLOWED_COMMANDS.contains(&cmd),
                "missing from allowlist: {}",
                cmd
            );
        }
    }

    #[test]
    fn allowlist_rejects_destructive() {
        for cmd in ["rm", "dd", "sudo", "mkfs", "shutdown"] {
            assert!(
                !ALLOWED_COMMANDS.contains(&cmd),
                "{} must not be allowlisted",
                cmd
            );
        }
    }

    /// A fresh, canonical scratch directory (the temp dir itself may be a symlink, e.g. on macOS,
    /// or an 8.3 short name on Windows), in the plain form `validate_cwd` returns.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtk-mcp-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        shell::plain_path(dir.canonicalize().unwrap())
    }

    /// Run `f` with RTK_MCP_CWD_JAIL set to `jail` (or unset), then put the variable back.
    /// `f` must not assert: a panic here would leave the variable set and poison the lock.
    fn with_jail<T>(jail: Option<&Path>, f: impl FnOnce() -> T) -> T {
        let _g = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var_os("RTK_MCP_CWD_JAIL");
        match jail {
            Some(jail) => std::env::set_var("RTK_MCP_CWD_JAIL", jail),
            None => std::env::remove_var("RTK_MCP_CWD_JAIL"),
        }
        let out = f();
        match saved {
            Some(v) => std::env::set_var("RTK_MCP_CWD_JAIL", v),
            None => std::env::remove_var("RTK_MCP_CWD_JAIL"),
        }
        out
    }

    #[test]
    fn cwd_without_a_jail_only_has_to_exist() {
        let dir = scratch_dir("nojail");
        let ok = with_jail(None, || validate_cwd(dir.to_str().unwrap()));
        assert_eq!(ok.unwrap(), dir);

        let gone = dir.join("nope");
        let err = with_jail(None, || validate_cwd(gone.to_str().unwrap())).unwrap_err();
        assert!(err.starts_with("invalid cwd"), "{}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd_jail_allows_the_root_and_everything_under_it() {
        let jail = scratch_dir("jail-in");
        let inner = jail.join("a/b");
        std::fs::create_dir_all(&inner).unwrap();
        for dir in [jail.clone(), jail.join("a"), inner.clone()] {
            let ok = with_jail(Some(&jail), || validate_cwd(dir.to_str().unwrap()))
                .unwrap_or_else(|e| panic!("{} should be allowed: {}", dir.display(), e));
            assert_eq!(ok, dir);
        }
        let _ = std::fs::remove_dir_all(&jail);
    }

    #[test]
    fn cwd_jail_blocks_escape() {
        let jail = scratch_dir("jail-out");
        // a sibling that merely shares the jail's name as a prefix is not inside it
        let sibling = PathBuf::from(format!("{}-sibling", jail.display()));
        std::fs::create_dir_all(&sibling).unwrap();
        let outside = [
            PathBuf::from("/"),
            jail.parent().unwrap().to_path_buf(),
            jail.join(".."),
            sibling.clone(),
        ];
        for dir in &outside {
            let err = with_jail(Some(&jail), || validate_cwd(dir.to_str().unwrap()))
                .expect_err(&format!("{} must be outside the jail", dir.display()));
            assert!(err.contains("outside allowed root"), "{}", err);
        }
        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&sibling);
    }

    #[cfg(unix)]
    #[test]
    fn cwd_jail_blocks_a_symlink_out() {
        let jail = scratch_dir("jail-link");
        let target = scratch_dir("jail-link-target");
        let link = jail.join("escape");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = with_jail(Some(&jail), || validate_cwd(link.to_str().unwrap())).unwrap_err();
        assert!(err.contains("outside allowed root"), "{}", err);
        let _ = std::fs::remove_dir_all(&jail);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn cwd_jail_with_a_missing_root_refuses_everything() {
        let dir = scratch_dir("jail-missing");
        let root = dir.join("gone");
        let err = with_jail(Some(&root), || validate_cwd(dir.to_str().unwrap())).unwrap_err();
        assert!(err.starts_with("invalid jail root"), "{}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RTK rewrite should transform known commands to their token-optimized form.
    #[tokio::test]
    async fn get_rtk_rewrite_known_command() {
        // Only run when RTK is actually available
        if detect_rtk().is_none() {
            return;
        }
        match get_rtk_rewrite("git status", None).await {
            Rewrite::Rewritten(r) => {
                assert!(r.contains("rtk"), "rewrite should include rtk: {}", r)
            }
            other => panic!("rtk should rewrite 'git status', got {:?}", other),
        }
    }

    /// RTK rewrite is a passthrough for commands RTK doesn't filter.
    #[tokio::test]
    async fn get_rtk_rewrite_unknown_command() {
        if detect_rtk().is_none() {
            return;
        }
        // `echo` is not an RTK subcommand — rewrite should leave it alone
        let rewritten = get_rtk_rewrite("echo hello world", None).await;
        assert_eq!(
            rewritten,
            Rewrite::Passthrough,
            "rtk should not rewrite 'echo'"
        );
    }

    /// RTK rewrite handles shell operators — rewrites the filterable parts.
    ///
    /// Where the `rtk` goes depends on the RTK version (`rtk cargo test | grep FAILED` before
    /// 0.49, `cargo test | rtk grep FAILED` since), so only the version-independent shape is
    /// asserted: still a pipeline over `cargo test`, and RTK is involved.
    #[tokio::test]
    async fn get_rtk_rewrite_pipe_command() {
        if detect_rtk().is_none() {
            return;
        }
        match get_rtk_rewrite("cargo test | grep FAILED", None).await {
            Rewrite::Rewritten(r) => {
                assert!(r.contains("rtk"), "should involve rtk: {}", r);
                assert!(r.contains("cargo test"), "should keep cargo test: {}", r);
                assert!(r.contains('|'), "should stay a pipeline: {}", r);
            }
            other => panic!("rtk should rewrite piped cargo test, got {:?}", other),
        }
    }

    /// A command that looks like an option must not be taken for `rtk rewrite --help`.
    #[tokio::test]
    async fn get_rtk_rewrite_option_like_command() {
        if detect_rtk().is_none() {
            return;
        }
        for cmd in ["--help", "--version", "-la"] {
            assert_eq!(
                get_rtk_rewrite(cmd, None).await,
                Rewrite::Passthrough,
                "{} must pass through untouched",
                cmd
            );
        }
    }

    #[test]
    fn interpret_rewrite_follows_exit_code_protocol() {
        let rewritten = |s: &str| Rewrite::Rewritten(s.to_string());
        // 0 = rewritten + allowed (RTK < 0.49), 3 = rewritten + ask/default (RTK 0.49+)
        assert_eq!(
            interpret_rewrite(Some(0), "rtk git status\n"),
            rewritten("rtk git status")
        );
        assert_eq!(
            interpret_rewrite(Some(3), "rtk git status\n"),
            rewritten("rtk git status")
        );
        // 1 = no RTK equivalent
        assert_eq!(interpret_rewrite(Some(1), ""), Rewrite::Passthrough);
        // 2 = deny rule — whatever is on stdout must be ignored
        assert_eq!(interpret_rewrite(Some(2), ""), Rewrite::Denied);
        assert_eq!(
            interpret_rewrite(Some(2), "rtk git push\n"),
            Rewrite::Denied
        );
        // an empty "rewrite" is no rewrite
        assert_eq!(interpret_rewrite(Some(0), "  \n"), Rewrite::Passthrough);
        // anything else (crash, usage error, killed by signal) must never be executed
        assert_eq!(
            interpret_rewrite(Some(1), "usage: rtk rewrite ..."),
            Rewrite::Passthrough
        );
        assert_eq!(
            interpret_rewrite(Some(101), "panic message"),
            Rewrite::Passthrough
        );
        assert_eq!(
            interpret_rewrite(None, "rtk git status"),
            Rewrite::Passthrough
        );
    }

    /// Write an executable stand-in for `rtk` that exits with `code` after printing `stdout`.
    #[cfg(unix)]
    fn stub_rtk(dir: &std::path::Path, code: i32, stdout: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("rtk-stub-{}", code));
        std::fs::write(
            &path,
            format!("#!/bin/sh\nprintf '%s' '{}'\nexit {}\n", stdout, code),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// The same as a batch file, which Windows runs through cmd.exe.
    #[cfg(windows)]
    fn stub_rtk(dir: &std::path::Path, code: i32, stdout: &str) -> String {
        let path = dir.join(format!("rtk-stub-{}.cmd", code));
        let print = if stdout.is_empty() {
            String::new()
        } else {
            format!("echo {}\r\n", stdout)
        };
        std::fs::write(&path, format!("@echo off\r\n{}exit /b {}\r\n", print, code)).unwrap();
        path.to_string_lossy().into_owned()
    }

    /// End to end through a real process: the exit code, not just stdout, decides.
    #[tokio::test]
    async fn rewrite_via_honours_exit_codes() {
        let dir = std::env::temp_dir().join(format!("rtk-mcp-stub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let allowed = stub_rtk(&dir, 0, "rtk git status");
        let ask = stub_rtk(&dir, 3, "rtk git status");
        let none = stub_rtk(&dir, 1, "");
        let deny = stub_rtk(&dir, 2, "");
        let crashed = stub_rtk(&dir, 101, "thread 'main' panicked");

        let rewritten = Rewrite::Rewritten("rtk git status".to_string());
        assert_eq!(rewrite_via(&allowed, "git status", None).await, rewritten);
        assert_eq!(rewrite_via(&ask, "git status", None).await, rewritten);
        assert_eq!(
            rewrite_via(&none, "git status", None).await,
            Rewrite::Passthrough
        );
        assert_eq!(rewrite_via(&deny, "git push", None).await, Rewrite::Denied);
        assert_eq!(
            rewrite_via(&crashed, "git status", None).await,
            Rewrite::Passthrough
        );
        assert_eq!(
            rewrite_via("/nonexistent/rtk", "git status", None).await,
            Rewrite::Passthrough,
            "a missing binary falls back to running the command as given"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_utf8_never_splits_a_character() {
        // 'é' is 2 bytes, '日' 3, '🦀' 4: every cut position below must stay on a boundary.
        let text = "aé日🦀".repeat(10);
        for max in 0..text.len() + 2 {
            let mut s = text.clone();
            truncate_utf8(&mut s, max);
            assert!(s.len() <= max);
            assert!(text.starts_with(&s));
        }
        let mut short = String::from("abc");
        truncate_utf8(&mut short, 10);
        assert_eq!(short, "abc");
    }

    /// Regression: multibyte output past the limit used to panic (and abort the server).
    #[tokio::test]
    async fn multibyte_output_over_limit_is_truncated_not_fatal() {
        // 3-byte characters, so a cut at 64 * 1024 (not divisible by 3) lands mid-character.
        let Some(command) = posix("yes 日本語日本語 | head -c 200000") else {
            return;
        };
        let limits = Limits {
            max_output: 64 * 1024,
            ..limits(60)
        };
        let result = exec_with(command, None, "test", limits).await.unwrap();
        assert!(
            result.output.contains("[output truncated at 64KB"),
            "len {}",
            result.output.len()
        );
        assert!(result.output.len() < 64 * 1024 + 100);
    }

    /// A runaway command must neither exhaust memory nor wedge the server.
    #[tokio::test]
    async fn runaway_output_is_capped() {
        let Some(command) = posix("head -c 50000000 /dev/zero | tr '\\0' x") else {
            return;
        };
        let result = exec_with(command, None, "test", limits(60)).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("[output truncated at 256KB"));
        assert!(result.output.len() < 256 * 1024 + 100);
    }

    /// RTK_MCP_MAX_OUTPUT_KB is what the cut is made at, whatever its size.
    #[tokio::test]
    async fn output_cap_follows_the_limit() {
        for kb in [1usize, 8, 100] {
            let Some(command) = posix("head -c 500000 /dev/zero | tr '\\0' x") else {
                return;
            };
            let limits = Limits {
                max_output: kb * 1024,
                ..limits(60)
            };
            let result = exec_with(command, None, "test", limits).await.unwrap();
            assert!(
                result
                    .output
                    .contains(&format!("[output truncated at {}KB", kb)),
                "{}KB: {}",
                kb,
                &result.output[result.output.len().saturating_sub(120)..]
            );
            assert!(result.output.starts_with(&"x".repeat(kb * 1024)));
            assert!(result.output.len() < kb * 1024 + 100);
        }
    }

    /// Output that fits under the cap comes back whole — nothing is cut or annotated.
    #[tokio::test]
    async fn output_under_the_cap_is_returned_whole() {
        let Some(command) = posix("head -c 200000 /dev/zero | tr '\\0' y") else {
            return;
        };
        let result = exec_with(command, None, "test", limits(60)).await.unwrap();
        assert_eq!(result.output, "y".repeat(200_000));
    }

    /// Both streams survive, and the exit code of a failing command is reported as before.
    #[tokio::test]
    async fn failing_command_keeps_both_streams_and_its_exit_code() {
        let Some(command) = posix("echo to-stdout; echo to-stderr >&2; exit 3") else {
            return;
        };
        let result = exec_with(command, None, "test", limits(60)).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.output, "to-stdout\n\n[stderr]\nto-stderr\n");
    }

    #[test]
    fn gain_args_maps_options() {
        let req = |json: &str| serde_json::from_str::<GetRtkSavingsRequest>(json).unwrap();
        assert_eq!(gain_args(&req("{}")), ["gain"]);
        assert_eq!(gain_args(&req(r#"{"graph":true}"#)), ["gain", "--graph"]);
        assert_eq!(
            gain_args(&req(r#"{"view":"all","format":"json","project":true}"#)),
            ["gain", "--all", "--project", "--format=json"]
        );
        assert_eq!(
            gain_args(&req(r#"{"view":"quota","tier":"5x"}"#)),
            ["gain", "--quota", "--tier=5x"]
        );
        // tier only means something for the quota view
        assert_eq!(
            gain_args(&req(r#"{"view":"daily","tier":"pro"}"#)),
            ["gain", "--daily"]
        );
        assert_eq!(gain_args(&req(r#"{"view":"summary"}"#)), ["gain"]);
        assert_eq!(
            gain_args(&req(r#"{"view":"recalls"}"#)),
            ["gain", "--recalls"]
        );
    }

    #[test]
    fn gain_args_cannot_reach_reset() {
        for view in ["reset", "--reset", "all --reset"] {
            let json = format!(r#"{{"view":"{}"}}"#, view);
            assert!(
                serde_json::from_str::<GetRtkSavingsRequest>(&json).is_err(),
                "view {:?} must be rejected",
                view
            );
        }
    }

    #[test]
    fn discover_args_maps_options() {
        let req = |json: &str| serde_json::from_str::<GetRtkDiscoverRequest>(json).unwrap();
        assert_eq!(discover_args(&req("{}")), ["discover"]);
        assert_eq!(
            discover_args(&req(r#"{"all":true,"since":7,"limit":5,"format":"json"}"#)),
            [
                "discover",
                "--all",
                "--since=7",
                "--limit=5",
                "--format=json"
            ]
        );
        // a value that looks like an option stays a value
        assert_eq!(
            discover_args(&req(r#"{"project":"--reset"}"#)),
            ["discover", "--project=--reset"]
        );
        assert!(serde_json::from_str::<GetRtkDiscoverRequest>(r#"{"format":"csv"}"#).is_err());
    }

    /// Regression test: child processes must not inherit the MCP server's stdin.
    #[tokio::test]
    async fn stdin_is_isolated_from_parent() {
        let Some(shell) = Shell::posix() else { return };
        let result = run_in_shell(&shell, "cat", None).await.unwrap();
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
        assert!(
            r2.output.contains("after"),
            "server must remain functional after a failed command"
        );
    }

    /// Alive means running: a killed-but-unreaped zombie (nobody reaps in some containers) is dead.
    #[cfg(unix)]
    fn process_alive(pid: i32) -> bool {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
            let after_comm = stat.rsplit(')').next().unwrap_or("");
            return !after_comm.trim_start().starts_with('Z');
        }
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Poll until `pid` is gone (SIGKILL is async); true if it died within ~5s.
    #[cfg(unix)]
    async fn dies_soon(pid: i32) -> bool {
        for _ in 0..50 {
            if !process_alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// A command whose shell parks a grandchild in the background and waits on it.
    #[cfg(unix)]
    fn tree_command(pidfile: &std::path::Path) -> Command {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("sleep 300 & echo $! > {}; wait", pidfile.display()),
        ]);
        command
    }

    #[cfg(unix)]
    fn pidfile(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rtk-mcp-{}-{}.pid", name, std::process::id()))
    }

    #[cfg(unix)]
    fn read_pid(pidfile: &std::path::Path) -> i32 {
        std::fs::read_to_string(pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// Regression: on timeout only `sh` used to die — the scanner it started kept running.
    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        let pidfile = pidfile("timeout");
        let started = std::time::Instant::now();
        let err = exec_with(tree_command(&pidfile), None, "test", limits(1))
            .await
            .err()
            .expect("must time out");
        assert!(err.contains("timed out after 1s"), "{}", err);
        assert!(started.elapsed() < Duration::from_secs(10));

        let grandchild = read_pid(&pidfile);
        assert!(
            dies_soon(grandchild).await,
            "grandchild {} survived the timeout",
            grandchild
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// Same when the client cancels the request: rmcp drops the handler future mid-flight.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_kills_the_whole_process_tree() {
        let pidfile = pidfile("cancel");
        let cancelled = timeout(
            Duration::from_millis(700),
            exec_with(tree_command(&pidfile), None, "test", limits(60)),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the call should still have been running"
        );

        let grandchild = read_pid(&pidfile);
        assert!(
            dies_soon(grandchild).await,
            "grandchild {} survived the cancel",
            grandchild
        );
        let _ = std::fs::remove_file(&pidfile);
    }

    /// A command that exits on its own may deliberately leave a background job — don't kill it.
    #[cfg(unix)]
    #[tokio::test]
    async fn normal_exit_leaves_deliberate_background_jobs_alone() {
        let result = run_shell_cmd("sleep 30 >/dev/null 2>&1 & echo $!", None)
            .await
            .unwrap();
        let pid: i32 = result.output.trim().parse().unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(process_alive(pid), "background job {} was killed", pid);
        // SAFETY: cleanup of the sleep started above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// `ssh -fN …`, `nohup server &`: a background job that keeps the output pipes open must
    /// neither hold the call until the timeout nor be killed when the call returns.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_job_holding_the_pipes_returns_promptly_and_survives() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & echo $!"]);
        let started = std::time::Instant::now();
        let result = exec_with(command, None, "test", limits(60)).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited {:?} on a background job",
            started.elapsed()
        );
        assert!(result.success);
        assert!(
            result.output.contains("background process"),
            "{}",
            result.output
        );
        let pid: i32 = result
            .output
            .lines()
            .next()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(process_alive(pid), "background job {} was killed", pid);
        // SAFETY: cleanup of the sleep started above.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    /// The job keeps writing after the call has returned. That must neither block it (full
    /// pipe) nor kill it (SIGPIPE): it has to run to its end.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_of_a_background_job_is_drained_not_sigpiped() {
        let marker = pidfile("drain");
        let _ = std::fs::remove_file(&marker);
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "( sleep 1; echo late; head -c 300000 /dev/zero; touch {} ) & echo started",
                marker.display()
            ),
        ]);
        let result = exec_with(command, None, "test", limits(60)).await.unwrap();
        assert!(result.output.starts_with("started"), "{}", result.output);
        for _ in 0..100 {
            if marker.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(marker.exists(), "the background job did not run to its end");
        let _ = std::fs::remove_file(&marker);
    }

    /// A timed-out scan still hands back what it had printed before the cutoff.
    #[tokio::test]
    async fn timeout_returns_the_output_printed_so_far() {
        let Some(command) = posix("echo found-port-80; echo noisy-warning >&2; sleep 60") else {
            return;
        };
        let err = exec_with(command, None, "test", limits(2))
            .await
            .err()
            .expect("must time out");
        assert!(err.starts_with("command timed out after 2s"), "{}", err);
        assert!(err.contains("found-port-80"), "{}", err);
        assert!(err.contains("noisy-warning"), "{}", err);

        // a command that never printed anything gets the bare timeout message
        let Some(silent) = posix("sleep 60") else {
            return;
        };
        let err = exec_with(silent, None, "test", limits(2))
            .await
            .err()
            .expect("must time out");
        assert_eq!(err, "command timed out after 2s");
    }

    /// A command line that prints `[<value of RTK_SUPPRESS_HOOK_WARNING>]` in the platform's shell.
    fn echo_hook_var() -> &'static str {
        match Shell::current() {
            Shell::Cmd(_) => "echo [%RTK_SUPPRESS_HOOK_WARNING%]",
            _ => "echo \"[$RTK_SUPPRESS_HOOK_WARNING]\"",
        }
    }

    /// The daily "no hook installed" nag must not reach the LLM-visible output.
    #[test]
    fn children_get_hook_warning_suppressed() {
        let _g = ENV_MUTEX.lock().unwrap();
        let saved = std::env::var_os("RTK_SUPPRESS_HOOK_WARNING");
        std::env::remove_var("RTK_SUPPRESS_HOOK_WARNING");

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let default = runtime
            .block_on(run_shell_cmd(echo_hook_var(), None))
            .unwrap();
        std::env::set_var("RTK_SUPPRESS_HOOK_WARNING", "0");
        let explicit = runtime
            .block_on(run_shell_cmd(echo_hook_var(), None))
            .unwrap();

        match saved {
            Some(v) => std::env::set_var("RTK_SUPPRESS_HOOK_WARNING", v),
            None => std::env::remove_var("RTK_SUPPRESS_HOOK_WARNING"),
        }
        assert_eq!(default.output.trim(), "[1]");
        assert_eq!(
            explicit.output.trim(),
            "[0]",
            "an explicit user setting must win"
        );
    }

    /// What the server told the model about itself before the Windows port. Linux and macOS
    /// still send exactly this.
    const INSTRUCTIONS_0_5_1: &str = r#"IMPORTANT: Use run_command for ALL shell execution instead of any built-in bash tool. RTK filters output to reduce token usage 60-90% for the commands it knows (git, cargo, go, docker, kubectl, aws, gh, curl, grep, find, ls, jq, pytest, ruff, mypy, tsc, vitest, rspec, rubocop and more, depending on the RTK version). Any other command runs as-is with full output: security scanners such as nmap, nuclei, httpx, subfinder, ffuf and sqlmap are not filtered, so cap their output yourself. Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported; commands run under `sh -c` (POSIX sh, not bash), so wrap bash-only syntax ({1..5}, [[ ]], <(...), &>) in `bash -c '...'`. Always pass 'cwd' so commands run in the right directory. Timeout is configurable via RTK_MCP_TIMEOUT_SECS (default 300s), the output cap via RTK_MCP_MAX_OUTPUT_KB (default 256KB). A background job (`cmd &`, `ssh -f`) must send its output to a file (`> log 2>&1 &`) or it is cut loose from the call after a few seconds. If RTK elides part of an output it prints a recall hint (`rtk recall <id>` or a saved-file path) — run it with run_command to get the full text. Prefix a command with RTK_DISABLED=1 to bypass filtering and get raw output."#;

    #[test]
    fn sh_instructions_are_what_0_5_1_sent() {
        assert_eq!(instructions(&Shell::Sh), INSTRUCTIONS_0_5_1);
    }

    #[test]
    fn instructions_name_the_windows_shells() {
        let bash = instructions(&Shell::Posix(PathBuf::from("bash.exe")));
        let cmd = instructions(&Shell::Cmd(PathBuf::from("cmd.exe")));
        for text in [&bash, &cmd] {
            assert!(text.starts_with(INSTRUCTIONS_HEAD), "{text}");
            assert!(text.ends_with(INSTRUCTIONS_TAIL), "{text}");
            assert!(!text.contains("`sh -c`"), "{text}");
        }
        assert!(bash.contains("POSIX shell on Windows"), "{bash}");
        assert!(cmd.contains("cmd.exe"), "{cmd}");
    }

    /// The descriptions say `sh -c` on Linux and macOS and are left alone there; on Windows
    /// `run_command` names the shell the server really uses.
    #[test]
    fn run_command_descriptions_follow_the_shell() {
        let describe = |shell: &Shell| {
            let mut router = RtkMcpServer::tool_router();
            describe_shell(&mut router, shell);
            let tool = router
                .get("run_command")
                .expect("run_command is registered");
            let param = tool.input_schema["properties"]["command"]["description"]
                .as_str()
                .expect("the command parameter is described")
                .to_string();
            (
                tool.description.as_deref().unwrap_or_default().to_string(),
                param,
            )
        };
        let (tool, param) = describe(&Shell::Sh);
        assert!(tool.contains(TOOL_SHELL_CLAUSE), "{tool}");
        assert!(param.contains(PARAM_SHELL_CLAUSE), "{param}");

        let bash = Shell::Posix(PathBuf::from("bash.exe"));
        let cmd = Shell::Cmd(PathBuf::from("cmd.exe"));
        for shell in [&bash, &cmd] {
            let (tool, param) = describe(shell);
            for text in [&tool, &param] {
                assert!(!text.contains("`sh -c`"), "{shell:?}: {text}");
                assert!(
                    text.contains("on this Windows machine"),
                    "{shell:?}: {text}"
                );
            }
            // the rest of each description is as it was
            assert!(tool.contains("Falls back to raw execution"), "{tool}");
            assert!(param.contains("Security scanners such as"), "{param}");
        }
    }

    #[test]
    fn windows_commands_are_allowed_only_on_windows() {
        for cmd in WINDOWS_COMMANDS {
            assert!(is_allowed_command(cmd, true), "{cmd}");
            assert!(
                !is_allowed_command(cmd, false),
                "{cmd} must stay out of the Unix allowlist"
            );
        }
        for cmd in ALLOWED_COMMANDS {
            assert!(
                is_allowed_command(cmd, true) && is_allowed_command(cmd, false),
                "{cmd}"
            );
        }
        for cmd in ["rm", "del", "rd", "format", "shutdown", "reg", "sudo"] {
            for windows in [false, true] {
                assert!(
                    !is_allowed_command(cmd, windows),
                    "{cmd} must not be allowlisted"
                );
            }
        }
    }

    /// Tests that need Windows itself: cmd.exe, `taskkill`, Windows paths.
    #[cfg(windows)]
    mod windows_only {
        use super::*;

        const HELPER: &str = "tests::windows_only::helper_sleep_and_report_pid";

        /// Not a test. The long-running child of the process-tree tests below: this test binary
        /// run on its own (`--ignored`), reporting its pid in $RTK_MCP_TEST_PIDFILE and sleeping.
        #[test]
        #[ignore = "helper process of the process-tree tests"]
        fn helper_sleep_and_report_pid() {
            if let Some(file) = std::env::var_os("RTK_MCP_TEST_PIDFILE") {
                std::fs::write(file, std::process::id().to_string()).unwrap();
                std::thread::sleep(Duration::from_secs(300));
            }
        }

        fn pidfile(name: &str) -> PathBuf {
            let path =
                std::env::temp_dir().join(format!("rtk-mcp-{}-{}.pid", name, std::process::id()));
            let _ = std::fs::remove_file(&path);
            path
        }

        /// cmd.exe running the helper: a two-level tree, like a shell running a scanner. With
        /// `background` the helper is started with `start /B` and cmd.exe exits at once.
        fn tree_command(pidfile: &Path, background: bool) -> Command {
            let exe = std::env::current_exe().unwrap();
            let helper = format!("\"{}\" --ignored --exact {HELPER}", exe.display());
            let line = if background {
                format!("start \"\" /B {helper}")
            } else {
                helper
            };
            let mut command = Shell::cmd().command(&line);
            command.env("RTK_MCP_TEST_PIDFILE", pidfile);
            Command::from(command)
        }

        fn process_alive(pid: u32) -> bool {
            let out = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .expect("tasklist runs");
            String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
        }

        /// Poll until `pid` is gone; true if it died within ~10s.
        async fn dies_soon(pid: u32) -> bool {
            for _ in 0..100 {
                if !process_alive(pid) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }

        /// Wait (up to 20s) for the helper to report its pid.
        async fn read_pid(pidfile: &Path) -> u32 {
            for _ in 0..200 {
                if let Ok(text) = std::fs::read_to_string(pidfile) {
                    if let Ok(pid) = text.trim().parse() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("the helper never reported its pid in {}", pidfile.display());
        }

        #[tokio::test]
        async fn timeout_kills_the_whole_process_tree() {
            let pidfile = pidfile("timeout");
            let started = std::time::Instant::now();
            let err = exec_with(tree_command(&pidfile, false), None, "test", limits(5))
                .await
                .err()
                .expect("must time out");
            assert!(err.contains("timed out after 5s"), "{}", err);
            assert!(started.elapsed() < Duration::from_secs(30));
            let child = read_pid(&pidfile).await;
            assert!(
                dies_soon(child).await,
                "child {} survived the timeout",
                child
            );
            let _ = std::fs::remove_file(&pidfile);
        }

        /// Same when the client cancels the request: rmcp drops the handler future mid-flight.
        #[tokio::test]
        async fn cancellation_kills_the_whole_process_tree() {
            let pidfile = pidfile("cancel");
            let mut call = Box::pin(exec_with(
                tree_command(&pidfile, false),
                None,
                "test",
                limits(120),
            ));
            let child = tokio::select! {
                _ = &mut call => panic!("the call ended on its own"),
                pid = read_pid(&pidfile) => pid,
            };
            drop(call);
            assert!(
                dies_soon(child).await,
                "child {} survived the cancel",
                child
            );
            let _ = std::fs::remove_file(&pidfile);
        }

        /// Git Bash starts a program by fork and exec, which leaves the new process without a
        /// living parent: `taskkill /T` cannot find it, the job object the command runs in can.
        #[tokio::test]
        async fn timeout_kills_what_git_bash_started() {
            let Some(shell) = Shell::posix() else { return };
            let pidfile = pidfile("posix-timeout");
            // the `sleep 1` first: /proc/$!/winpid names the forked shell until the exec is done
            let line = format!(
                "sleep 300 & sleep 1; cat /proc/$!/winpid > '{}'; wait",
                pidfile.display().to_string().replace('\\', "/")
            );
            let err = exec_with(Command::from(shell.command(&line)), None, "test", limits(5))
                .await
                .err()
                .expect("must time out");
            assert!(err.contains("timed out after 5s"), "{}", err);
            let sleeper = read_pid(&pidfile).await;
            assert!(
                dies_soon(sleeper).await,
                "sleep {} survived the timeout",
                sleeper
            );
            let _ = std::fs::remove_file(&pidfile);
        }

        /// A command that ends by itself may leave a job behind on purpose: the call neither
        /// waits for it nor kills it.
        #[tokio::test]
        async fn a_background_job_is_left_alone_when_cmd_exits() {
            let pidfile = pidfile("background");
            let started = std::time::Instant::now();
            let result = exec_with(tree_command(&pidfile, true), None, "test", limits(60))
                .await
                .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "waited {:?} on a background job",
                started.elapsed()
            );
            assert!(result.success, "{}", result.output);
            let job = read_pid(&pidfile).await;
            assert!(process_alive(job), "background job {} was killed", job);
            shell::kill_tree(job);
            let _ = std::fs::remove_file(&pidfile);
        }

        /// The same through Git Bash: `sleep &` keeps the output pipes open, and the call returns
        /// anyway, with the job still running.
        #[tokio::test]
        async fn a_posix_shell_background_job_returns_promptly_and_survives() {
            let Some(shell) = Shell::posix() else { return };
            let started = std::time::Instant::now();
            // /proc/$!/winpid is the Windows pid of the job
            let result = run_in_shell(&shell, "sleep 60 & cat /proc/$!/winpid", None)
                .await
                .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "waited {:?} on a background job",
                started.elapsed()
            );
            assert!(result.success, "{}", result.output);
            assert!(
                result.output.contains("background process"),
                "{}",
                result.output
            );
            let job: u32 = result
                .output
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .parse()
                .unwrap_or_else(|_| panic!("no pid in {:?}", result.output));
            assert!(process_alive(job), "background job {} was killed", job);
            shell::kill_tree(job);
        }

        #[tokio::test]
        async fn cmd_expands_variables_and_applies_leading_assignments() {
            let cmd = Shell::cmd();
            // unset: cmd leaves %NAME% as it is
            let unset = run_in_shell(&cmd, "echo [%RTK_MCP_PROBE%]", None)
                .await
                .unwrap();
            assert_eq!(unset.output.trim(), "[%RTK_MCP_PROBE%]");
            let set = run_in_shell(&cmd, "RTK_MCP_PROBE=42 echo [%RTK_MCP_PROBE%]", None)
                .await
                .unwrap();
            assert_eq!(set.output.trim(), "[42]");
        }

        /// Not a test. The `cmd.exe` launcher as a process of its own: this test binary run on its
        /// own (`--ignored`), running $RTK_MCP_TEST_LAUNCH_LINE the way `rtk-mcp.exe
        /// --rtk-mcp-cmd-launcher` does.
        #[test]
        #[ignore = "helper process of cmd_launcher_prints_utf8"]
        fn helper_cmd_launcher() {
            if let Some(line) = std::env::var_os("RTK_MCP_TEST_LAUNCH_LINE") {
                let Shell::Cmd(cmd) = Shell::cmd() else {
                    unreachable!("Shell::cmd is cmd.exe")
                };
                std::process::exit(shell::run_cmd_launcher(cmd.as_os_str(), &line));
            }
        }

        /// What cmd.exe prints for its own commands is in the code page of the console it starts
        /// in, so the launcher sets it before it starts cmd. `CREATE_NO_WINDOW` gives the launcher
        /// a new console in the OEM code page, as it has when the server starts it: the case that
        /// matters.
        #[test]
        fn cmd_launcher_prints_utf8_and_passes_the_exit_code() {
            use std::os::windows::process::CommandExt;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "tests::windows_only::helper_cmd_launcher",
                ])
                .env(
                    "RTK_MCP_TEST_LAUNCH_LINE",
                    "echo héllo 日本語 العربية& exit /b 7",
                )
                .stdin(Stdio::null())
                .creation_flags(shell::CREATE_NO_WINDOW)
                .output()
                .expect("the launcher runs");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("héllo 日本語 العربية"), "{stdout:?}");
            assert_eq!(output.status.code(), Some(7), "{stdout:?}");
        }

        #[tokio::test]
        async fn cmd_reports_the_exit_code_and_both_streams() {
            let line = "echo to-stdout& echo to-stderr 1>&2& exit /b 3";
            let result = run_in_shell(&Shell::cmd(), line, None).await.unwrap();
            assert!(!result.success);
            assert_eq!(result.exit_code, 3);
            assert!(result.output.contains("to-stdout"), "{}", result.output);
            assert!(result.output.contains("[stderr]"), "{}", result.output);
            assert!(result.output.contains("to-stderr"), "{}", result.output);
        }

        /// `fs::canonicalize` returns `\\?\C:\…`, which cmd.exe refuses as a working directory.
        #[tokio::test]
        async fn cwd_reaches_cmd_and_bash_as_a_plain_path() {
            let dir = scratch_dir("win cwd");
            let cwd = with_jail(None, || validate_cwd(dir.to_str().unwrap())).unwrap();
            assert!(
                !cwd.to_string_lossy().starts_with(r"\\?\"),
                "{}",
                cwd.display()
            );

            let shown = run_in_shell(&Shell::cmd(), "cd", Some(&cwd)).await.unwrap();
            assert!(
                shown
                    .output
                    .trim()
                    .eq_ignore_ascii_case(&cwd.to_string_lossy()),
                "{} vs {}",
                shown.output,
                cwd.display()
            );

            if let Some(shell) = Shell::posix() {
                // `pwd -W` prints the Windows form of the directory, with forward slashes
                let shown = run_in_shell(&shell, "pwd -W", Some(&cwd)).await.unwrap();
                let want = cwd.to_string_lossy().replace('\\', "/");
                assert!(
                    shown.output.trim().eq_ignore_ascii_case(&want),
                    "{} vs {}",
                    shown.output,
                    want
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
