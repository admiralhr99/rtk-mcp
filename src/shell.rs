//! The shell a command string is handed to, and the platform details around running it.
//!
//! Linux and macOS use `sh -c`, as they always did. Windows has no `sh`, so the shell is chosen
//! once: Git Bash when Git for Windows is installed (POSIX syntax, the same as on Linux), otherwise
//! `cmd.exe`. On Windows `RTK_MCP_SHELL` overrides the choice: `auto` (the default), `bash`, `cmd`,
//! or the path of a POSIX shell. The variable is ignored elsewhere.
//!
//! Nearly everything here is plain string and path logic that compiles, and is unit-tested, on every
//! platform. Only the few calls into Windows itself are `cfg(windows)`.

// The Windows logic is compiled, and unit-tested, everywhere; off Windows only the tests use it.
#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// `CreateProcess` flag: run a console program without a console window. Without it a GUI client
/// such as Claude Desktop would see a window flash up for every command.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The shell that runs a command string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shell {
    /// `sh -c`: POSIX sh (Linux, macOS).
    #[cfg_attr(windows, allow(dead_code))]
    Sh,
    /// A POSIX shell on Windows, run as `<path> -c <command>`: Git Bash, or the shell that
    /// `RTK_MCP_SHELL` names.
    Posix(PathBuf),
    /// `cmd.exe /D /S /C` (Windows).
    Cmd(PathBuf),
}

impl Shell {
    /// The shell for this process, chosen on first use.
    pub fn current() -> &'static Shell {
        static CURRENT: OnceLock<Shell> = OnceLock::new();
        CURRENT.get_or_init(|| {
            let shell = Shell::detect();
            tracing::info!("commands run under {}", shell.label());
            shell
        })
    }

    #[cfg(not(windows))]
    fn detect() -> Shell {
        Shell::Sh
    }

    #[cfg(windows)]
    fn detect() -> Shell {
        choose_windows_shell(std::env::var("RTK_MCP_SHELL").ok().as_deref(), &RealMachine)
    }

    /// `cmd.exe` as this machine has it (what `auto` falls back to without Git). For tests.
    #[cfg(test)]
    pub fn cmd() -> Shell {
        Shell::Cmd(cmd_exe(&RealMachine))
    }

    /// A POSIX shell for tests: `sh`, or Git Bash on Windows (`None` without Git).
    #[cfg(test)]
    pub fn posix() -> Option<Shell> {
        #[cfg(not(windows))]
        {
            Some(Shell::Sh)
        }
        #[cfg(windows)]
        {
            git_bash(&RealMachine).map(Shell::Posix)
        }
    }

    /// How the shell is invoked, for logs.
    pub fn label(&self) -> String {
        match self {
            Shell::Sh => "sh -c".to_string(),
            Shell::Posix(path) => format!("{} -c", path.display()),
            Shell::Cmd(path) => format!("{} /D /S /C", path.display()),
        }
    }

    /// A `std` command that runs `line` in this shell.
    pub fn command(&self, line: &str) -> Command {
        match self {
            Shell::Sh => {
                let mut command = Command::new("sh");
                command.args(["-c", line]);
                command
            }
            Shell::Posix(path) => posix_command(
                path,
                line,
                &posix_tool_dirs(path, &RealMachine),
                std::env::var_os("PATH").as_deref(),
            ),
            Shell::Cmd(path) => cmd_command(path, line),
        }
    }

    /// The clause that replaces "runs under `sh -c` …" in the `run_command` descriptions; `None`
    /// for `sh`, whose descriptions the source already carries.
    pub fn runs_under(&self) -> Option<String> {
        match self {
            Shell::Sh => None,
            Shell::Posix(path) => Some(format!(
                "runs under a POSIX shell on this Windows machine ({}): write the same syntax as on \
                 Linux; Windows drive paths work as C:/dir or /c/dir, and flags of native programs \
                 need a doubled slash (`taskkill //F`).",
                path.display()
            )),
            Shell::Cmd(_) => Some(
                "runs under cmd.exe on this Windows machine: write Windows syntax (dir, type, \
                 findstr, %VAR%, &&, |, 2>&1) on one line; for PowerShell run `powershell \
                 -NoProfile -Command \"...\"`."
                    .to_string(),
            ),
        }
    }

    /// What the server tells the model about the syntax it should write. For `sh` this is the text
    /// the server has always sent.
    pub fn syntax_note(&self) -> String {
        match self {
            Shell::Sh => "Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported; commands run \
                 under `sh -c` (POSIX sh, not bash), so wrap bash-only syntax ({1..5}, [[ ]], <(...), &>) \
                 in `bash -c '...'`."
                .to_string(),
            Shell::Posix(path) => format!(
                "Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported; commands run under a \
                 POSIX shell on Windows ({}), so write the same syntax as on Linux. Windows drive paths \
                 work as C:/dir or /c/dir. The shell rewrites arguments that look like POSIX paths, so \
                 double the slash for flags such as `taskkill //F //PID 123`.",
                path.display()
            ),
            Shell::Cmd(_) => "Commands run under cmd.exe, so write Windows syntax: dir, type, findstr, \
                 %VAR%, &&, |, 2>&1. Bash syntax ($VAR, heredocs, ls, grep) does not work, and a command \
                 must fit on one line. A leading NAME=value (such as RTK_DISABLED=1) is applied to the \
                 command's environment. For PowerShell run `powershell -NoProfile -Command \"...\"`; \
                 installing Git for Windows gives POSIX syntax instead."
                .to_string(),
        }
    }
}

/// `cmd.exe /D /S /C "chcp 65001 >nul & <line>"`.
///
/// `/D` skips AutoRun commands, and `/S` makes cmd strip exactly the outer quotes added here, so
/// `line` reaches it untouched (`raw_arg`: no Rust quoting on top). `chcp 65001` switches the
/// console to UTF-8 so non-ASCII output survives. cmd has no `NAME=value command` prefix, so those
/// words (`RTK_DISABLED=1 git status`) are moved into the environment instead.
fn cmd_command(cmd: &Path, line: &str) -> Command {
    let (assignments, rest) = split_env_prefix(line);
    let mut command = Command::new(cmd);
    command.args(["/D", "/S", "/C"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.raw_arg(cmd_script(rest));
    }
    #[cfg(not(windows))]
    command.arg(cmd_script(rest));
    for (name, value) in assignments {
        command.env(name, value);
    }
    command
}

/// The argument that follows `/S /C`.
fn cmd_script(line: &str) -> String {
    format!("\"chcp 65001 >nul & {}\"", line)
}

/// The leading `NAME=value` words of a POSIX-style command line, and the rest of the line.
///
/// Values are bare words or quoted with `'` or `"`. A line that is nothing but assignments, or
/// whose first word only looks like one (`A="b"c`), is left alone.
pub fn split_env_prefix(line: &str) -> (Vec<(String, String)>, &str) {
    let mut assignments = Vec::new();
    let mut rest = line.trim_start();
    while let Some((name, value, after)) = parse_assignment(rest) {
        assignments.push((name, value));
        rest = after.trim_start();
    }
    if rest.is_empty() {
        return (Vec::new(), line);
    }
    (assignments, rest)
}

/// One `NAME=value` word at the start of `s`: (name, value, text after the word).
fn parse_assignment(s: &str) -> Option<(String, String, &str)> {
    let eq = s.find('=')?;
    let name = &s[..eq];
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let body = &s[eq + 1..];
    let (value, after) = match body.chars().next() {
        Some(quote @ ('\'' | '"')) => {
            let close = body[1..].find(quote)?;
            (&body[1..1 + close], &body[close + 2..])
        }
        _ => {
            let end = body.find(char::is_whitespace).unwrap_or(body.len());
            (&body[..end], &body[end..])
        }
    };
    // the word has to end here: `A="b"c` is not a plain assignment
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    Some((name.to_string(), value.to_string(), after))
}

/// What shell discovery needs to know about the machine (a fake in the tests).
pub trait Machine {
    fn var(&self, name: &str) -> Option<OsString>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
}

/// The machine this process runs on.
pub struct RealMachine;

impl Machine for RealMachine {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

/// The shell for Windows given the value of `RTK_MCP_SHELL`.
///
/// `auto` (or unset): Git Bash if Git for Windows is installed, else cmd.exe. `bash`: the same
/// Git Bash, falling back to cmd.exe with a warning. `cmd`: cmd.exe. Anything else is taken as the
/// path of a POSIX shell. Windows' own `System32\bash.exe` is never used: it starts WSL.
pub fn choose_windows_shell(setting: Option<&str>, machine: &dyn Machine) -> Shell {
    let setting = setting
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto");
    let auto = || match git_bash(machine) {
        Some(bash) => Shell::Posix(bash),
        None => Shell::Cmd(cmd_exe(machine)),
    };
    match setting.to_ascii_lowercase().as_str() {
        "auto" => auto(),
        "cmd" => Shell::Cmd(cmd_exe(machine)),
        "bash" => match git_bash(machine) {
            Some(bash) => Shell::Posix(bash),
            None => {
                tracing::warn!("RTK_MCP_SHELL=bash, but Git Bash was not found — using cmd.exe");
                Shell::Cmd(cmd_exe(machine))
            }
        },
        _ => {
            let path = PathBuf::from(setting);
            if machine.is_file(&path) {
                Shell::Posix(path)
            } else {
                tracing::warn!(
                    "RTK_MCP_SHELL={setting} is not a shell I know or a file — using auto"
                );
                auto()
            }
        }
    }
}

/// `%COMSPEC%`, else `%SystemRoot%\System32\cmd.exe`, else whatever `cmd.exe` the search finds.
fn cmd_exe(machine: &dyn Machine) -> PathBuf {
    if let Some(comspec) = machine.var("ComSpec") {
        let path = PathBuf::from(comspec);
        if machine.is_file(&path) {
            return path;
        }
    }
    if let Some(root) = machine.var("SystemRoot") {
        let path = Path::new(&root).join("System32").join("cmd.exe");
        if machine.is_file(&path) {
            return path;
        }
    }
    PathBuf::from("cmd.exe")
}

/// `bin\bash.exe` of the Git for Windows installation, if there is one.
pub fn git_bash(machine: &dyn Machine) -> Option<PathBuf> {
    git_bash_candidates(machine)
        .into_iter()
        .find(|path| machine.is_file(path))
}

/// Where Git for Windows keeps its `bash.exe`: the usual install directories, and the layout around
/// any `git.exe` on PATH (`…\Git\cmd`, `…\Git\bin` or `…\Git\mingw64\bin`).
fn git_bash_candidates(machine: &dyn Machine) -> Vec<PathBuf> {
    fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
        if !list.contains(&path) {
            list.push(path);
        }
    }
    let mut found = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(dir) = machine.var(var) {
            push_unique(
                &mut found,
                Path::new(&dir).join("Git").join("bin").join("bash.exe"),
            );
        }
    }
    if let Some(dir) = machine.var("LocalAppData") {
        push_unique(
            &mut found,
            Path::new(&dir)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    if let Some(path) = machine.var("PATH") {
        for dir in std::env::split_paths(&path) {
            if !machine.is_file(&dir.join("git.exe")) {
                continue;
            }
            push_unique(&mut found, dir.join("bash.exe"));
            if let Some(parent) = dir.parent() {
                push_unique(&mut found, parent.join("bin").join("bash.exe"));
                if let Some(grandparent) = parent.parent() {
                    push_unique(&mut found, grandparent.join("bin").join("bash.exe"));
                }
            }
        }
    }
    found
}

/// `\\?\C:\dir` → `C:\dir`, `\\?\UNC\host\share` → `\\host\share`; `None` for anything else.
///
/// `fs::canonicalize` returns the `\\?\` form on Windows, and cmd.exe refuses it as a working
/// directory (it quietly starts in the Windows directory instead).
pub fn strip_verbatim_prefix(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{rest}"));
    }
    let rest = path.strip_prefix(r"\\?\")?;
    let bytes = rest.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if drive && (bytes.len() == 2 || bytes[2] == b'\\') {
        Some(rest.to_string())
    } else {
        None
    }
}

/// The form of a canonical path that child processes can use as their working directory.
pub fn plain_path(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    if let Some(plain) = path.to_str().and_then(strip_verbatim_prefix) {
        return PathBuf::from(plain);
    }
    path
}

/// `PATH` with the `first` directories at the front, in that order, and not repeated further down.
pub fn path_with_dirs_first(path: Option<&OsStr>, first: &[PathBuf]) -> OsString {
    let mut dirs: Vec<PathBuf> = first.to_vec();
    if let Some(path) = path {
        dirs.extend(std::env::split_paths(path).filter(|p| !first.contains(p)));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| path.map(OsStr::to_os_string).unwrap_or_default())
}

/// Where a POSIX shell keeps its own tools (`ls`, `tr`, `sort`, …) when it ships with a userland,
/// as Git for Windows and MSYS2 do: `<root>\usr\bin`. A login shell puts that directory on PATH
/// (`/etc/profile`); a `bash -c` started by a Windows program may not, and then `sort` and `find`
/// would be Windows' own programs of the same names.
pub fn posix_tool_dirs(shell: &Path, machine: &dyn Machine) -> Vec<PathBuf> {
    let named = |path: &Path, name: &str| {
        path.file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    };
    let Some(bin) = shell.parent().filter(|bin| named(bin, "bin")) else {
        return Vec::new();
    };
    let Some(root) = bin.parent() else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    // <root>\usr\bin\bash.exe: the shell sits in the tools directory itself
    if named(root, "usr") {
        dirs.push(bin.to_path_buf());
    }
    // <root>\bin\bash.exe: the tools are next door
    let usr_bin = root.join("usr").join("bin");
    if !dirs.contains(&usr_bin) && machine.is_dir(&usr_bin) {
        dirs.push(usr_bin);
    }
    dirs
}

/// `<shell> -c <line>`, with `tools` in front of `path` (the PATH the shell would inherit).
fn posix_command(shell: &Path, line: &str, tools: &[PathBuf], path: Option<&OsStr>) -> Command {
    let mut command = Command::new(shell);
    command.args(["-c", line]);
    if !tools.is_empty() {
        command.env("PATH", path_with_dirs_first(path, tools));
    }
    command
}

/// The directory of `rtk.exe` when it sits next to `rtk-mcp.exe`, which is how the Windows
/// installer lays them out. A Windows process looks in its own directory first, so the probes find
/// it; the shell only searches PATH, so the children get this directory put on theirs.
pub fn bundled_rtk_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        bundled_rtk_dir_of(&std::env::current_exe().ok()?)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The directory of `exe`, if an `rtk.exe` sits in it.
fn bundled_rtk_dir_of(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    dir.join("rtk.exe").is_file().then(|| dir.to_path_buf())
}

/// The program a command line starts with, as written, for the allowlist.
///
/// On Linux and macOS that is the first word under POSIX quoting rules. On Windows the first word
/// is often a path with backslashes (`C:\Windows\System32\hostname.exe`,
/// `"C:\Program Files\Git\cmd\git.exe" status`), which POSIX splitting would eat, so there the word
/// ends at the first white space, or at the closing quote of a quoted one, and nothing is unescaped.
pub fn program_of(command: &str, windows: bool) -> Result<String, String> {
    const EMPTY: &str = "Error: empty command after parsing";
    if windows {
        let line = command.trim_start();
        let word = match line.chars().next() {
            Some(quote @ ('"' | '\'')) => line[1..].split(quote).next().unwrap_or_default(),
            _ => line.split_whitespace().next().unwrap_or_default(),
        };
        return if word.is_empty() {
            Err(EMPTY.to_string())
        } else {
            Ok(word.to_string())
        };
    }
    let parts = shlex::split(command)
        .ok_or_else(|| "Failed to parse command: unmatched quotes".to_string())?;
    parts.into_iter().next().ok_or_else(|| EMPTY.to_string())
}

/// The command name in the first word of a command line, for the allowlist: no directory, and on
/// Windows no `.exe` and no case.
pub fn base_command(first: &str, windows: bool) -> String {
    if !windows {
        return first.rsplit('/').next().unwrap_or(first).to_string();
    }
    let name = first.rsplit(['/', '\\']).next().unwrap_or(first);
    let name = match name.len().checked_sub(4) {
        Some(cut) if name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(".exe") => {
            &name[..cut]
        }
        _ => name,
    };
    name.to_ascii_lowercase()
}

/// Kill `pid` and everything it started (Windows has no process groups to signal).
///
/// `taskkill /T` walks the tree by parent id, so it reaches the whole tree as long as the shell is
/// alive, which it is when a timeout or a cancel calls this.
#[cfg(windows)]
pub fn kill_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let taskkill = Path::new(&system_root)
        .join("System32")
        .join("taskkill.exe");
    let program = if taskkill.is_file() {
        taskkill
    } else {
        PathBuf::from("taskkill")
    };
    let _ = Command::new(program)
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine described by a list of variables and files.
    struct Fake {
        vars: Vec<(&'static str, OsString)>,
        files: Vec<PathBuf>,
    }

    impl Machine for Fake {
        fn var(&self, name: &str) -> Option<OsString> {
            self.vars
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|file| file == path)
        }

        /// A directory exists when a file lives in it.
        fn is_dir(&self, path: &Path) -> bool {
            self.files
                .iter()
                .any(|file| file != path && file.starts_with(path))
        }
    }

    fn pf_bash() -> PathBuf {
        Path::new("pf").join("Git").join("bin").join("bash.exe")
    }

    fn windows_machine(with_git: bool) -> Fake {
        let mut files = vec![Path::new("win").join("System32").join("cmd.exe")];
        if with_git {
            files.push(pf_bash());
        }
        Fake {
            vars: vec![("ProgramFiles", "pf".into()), ("SystemRoot", "win".into())],
            files,
        }
    }

    fn assignments(line: &str) -> (Vec<(String, String)>, String) {
        let (list, rest) = split_env_prefix(line);
        (list, rest.to_string())
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn env_prefix_is_split_off() {
        assert_eq!(
            assignments("RTK_DISABLED=1 git status"),
            (vec![pair("RTK_DISABLED", "1")], "git status".to_string())
        );
        assert_eq!(
            assignments("  A=1 B=two   cargo test -- --nocapture"),
            (
                vec![pair("A", "1"), pair("B", "two")],
                "cargo test -- --nocapture".to_string()
            )
        );
        assert_eq!(
            assignments(r#"FOO="a b" BAR='c d' echo hi"#),
            (
                vec![pair("FOO", "a b"), pair("BAR", "c d")],
                "echo hi".to_string()
            )
        );
        assert_eq!(
            assignments("EMPTY= run"),
            (vec![pair("EMPTY", "")], "run".to_string())
        );
        assert_eq!(
            assignments(r"PATH=C:\tools;%PATH% git log"),
            (
                vec![pair("PATH", r"C:\tools;%PATH%")],
                "git log".to_string()
            )
        );
    }

    #[test]
    fn env_prefix_leaves_everything_else_alone() {
        for line in [
            "git status",
            "echo a=b",
            "set X=1 && echo %X%",
            "1A=2 cmd",
            "A-B=2 cmd",
            "=x cmd",
            r#"A="unterminated cmd"#,
            r#"A="b"c cmd"#,
            "dir C:\\x=y",
        ] {
            assert_eq!(
                assignments(line),
                (Vec::new(), line.trim_start().to_string()),
                "{line:?}"
            );
        }
        // nothing but assignments: the line is not touched, the shell reports it
        assert_eq!(assignments("FOO=1"), (Vec::new(), "FOO=1".to_string()));
        assert_eq!(assignments("A=1 B=2"), (Vec::new(), "A=1 B=2".to_string()));
    }

    #[test]
    fn cmd_script_wraps_the_line_in_the_quotes_that_slash_s_strips() {
        assert_eq!(cmd_script("dir"), "\"chcp 65001 >nul & dir\"");
        assert_eq!(
            cmd_script(r#"findstr "a b" f.txt"#),
            r#""chcp 65001 >nul & findstr "a b" f.txt""#
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn cmd_command_arguments_and_environment() {
        let command = cmd_command(Path::new("cmd.exe"), "RTK_DISABLED=1 git status");
        assert_eq!(command.get_program(), "cmd.exe");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["/D", "/S", "/C", "\"chcp 65001 >nul & git status\""]);
        let env: Vec<_> = command.get_envs().collect();
        assert_eq!(env, [(OsStr::new("RTK_DISABLED"), Some(OsStr::new("1")))]);
    }

    #[test]
    fn posix_shells_get_the_line_verbatim() {
        let sh = Shell::Sh.command("RTK_DISABLED=1 echo \"$HOME\"");
        assert_eq!(sh.get_program(), "sh");
        assert_eq!(
            sh.get_args().collect::<Vec<_>>(),
            ["-c", "RTK_DISABLED=1 echo \"$HOME\""]
        );
        assert_eq!(sh.get_envs().count(), 0, "sh handles the prefix itself");

        let bash = Shell::Posix(pf_bash()).command("ls");
        assert_eq!(bash.get_program(), pf_bash().as_os_str());
        assert_eq!(bash.get_args().collect::<Vec<_>>(), ["-c", "ls"]);
    }

    #[test]
    fn sh_instructions_are_the_text_the_server_always_sent() {
        assert_eq!(
            Shell::Sh.syntax_note(),
            "Shell operators (|, &&, ||, ;, >, <, $VAR) are fully supported; commands run under \
             `sh -c` (POSIX sh, not bash), so wrap bash-only syntax ({1..5}, [[ ]], <(...), &>) \
             in `bash -c '...'`."
        );
        assert_eq!(Shell::Sh.label(), "sh -c");
    }

    #[test]
    fn windows_notes_name_the_shell() {
        let bash = Shell::Posix(pf_bash()).syntax_note();
        assert!(bash.contains("POSIX shell"), "{bash}");
        assert!(bash.contains("bash.exe"), "{bash}");
        let cmd = Shell::Cmd(PathBuf::from("cmd.exe")).syntax_note();
        assert!(cmd.contains("cmd.exe") && cmd.contains("%VAR%"), "{cmd}");
        assert!(cmd.contains("RTK_DISABLED=1"), "{cmd}");
    }

    #[test]
    fn auto_prefers_git_bash_and_falls_back_to_cmd() {
        let with = windows_machine(true);
        for setting in [None, Some(""), Some("  "), Some("auto"), Some("AUTO")] {
            assert_eq!(
                choose_windows_shell(setting, &with),
                Shell::Posix(pf_bash()),
                "{setting:?}"
            );
        }
        let without = windows_machine(false);
        let cmd = Shell::Cmd(Path::new("win").join("System32").join("cmd.exe"));
        assert_eq!(choose_windows_shell(None, &without), cmd);
        assert_eq!(choose_windows_shell(Some("auto"), &without), cmd);
    }

    #[test]
    fn an_explicit_shell_wins() {
        let with = windows_machine(true);
        let cmd = Shell::Cmd(Path::new("win").join("System32").join("cmd.exe"));
        assert_eq!(choose_windows_shell(Some("cmd"), &with), cmd);
        assert_eq!(choose_windows_shell(Some("CMD"), &with), cmd);
        assert_eq!(
            choose_windows_shell(Some("bash"), &with),
            Shell::Posix(pf_bash())
        );
        // bash was asked for but is not installed: cmd.exe, not a failure
        assert_eq!(
            choose_windows_shell(Some("bash"), &windows_machine(false)),
            cmd
        );
    }

    #[test]
    fn a_path_names_a_posix_shell_when_it_exists() {
        let mut machine = windows_machine(true);
        let msys = Path::new("msys64").join("usr").join("bin").join("bash.exe");
        machine.files.push(msys.clone());
        assert_eq!(
            choose_windows_shell(msys.to_str(), &machine),
            Shell::Posix(msys)
        );
        // not a file: the setting is ignored, auto applies
        assert_eq!(
            choose_windows_shell(Some("nowhere/zsh.exe"), &machine),
            Shell::Posix(pf_bash())
        );
    }

    #[test]
    fn cmd_exe_is_found_through_comspec_then_systemroot() {
        let comspec = PathBuf::from("elsewhere").join("cmd.exe");
        let mut machine = windows_machine(false);
        machine.vars.push(("ComSpec", comspec.clone().into()));
        machine.files.push(comspec.clone());
        assert_eq!(cmd_exe(&machine), comspec);
        // a ComSpec that does not exist is skipped
        machine.files.retain(|f| f != &comspec);
        assert_eq!(
            cmd_exe(&machine),
            Path::new("win").join("System32").join("cmd.exe")
        );
        // nothing known: let the search find it
        let bare = Fake {
            vars: Vec::new(),
            files: Vec::new(),
        };
        assert_eq!(cmd_exe(&bare), PathBuf::from("cmd.exe"));
    }

    #[test]
    fn git_bash_is_looked_for_around_git_exe_on_path() {
        let cmd_dir = Path::new("tools").join("Git").join("cmd");
        let bash = Path::new("tools").join("Git").join("bin").join("bash.exe");
        let machine = Fake {
            vars: vec![(
                "PATH",
                std::env::join_paths([Path::new("other"), cmd_dir.as_path()]).unwrap(),
            )],
            files: vec![cmd_dir.join("git.exe"), bash.clone()],
        };
        assert_eq!(git_bash(&machine), Some(bash));
    }

    #[test]
    fn git_bash_is_found_in_the_standard_places() {
        let local = Path::new("local")
            .join("Programs")
            .join("Git")
            .join("bin")
            .join("bash.exe");
        let machine = Fake {
            vars: vec![("LocalAppData", "local".into())],
            files: vec![local.clone()],
        };
        assert_eq!(git_bash(&machine), Some(local));
        let nothing = Fake {
            vars: Vec::new(),
            files: Vec::new(),
        };
        assert_eq!(git_bash(&nothing), None);
    }

    #[test]
    fn wsl_launcher_is_never_a_candidate() {
        let system32 = Path::new("win").join("System32");
        let machine = Fake {
            vars: vec![
                ("SystemRoot", "win".into()),
                ("PATH", std::env::join_paths([system32.as_path()]).unwrap()),
            ],
            files: vec![system32.join("bash.exe")],
        };
        assert_eq!(git_bash(&machine), None);
    }

    #[test]
    fn verbatim_prefixes_are_stripped_only_where_there_is_a_plain_form() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\Users\me\proj").as_deref(),
            Some(r"C:\Users\me\proj")
        );
        assert_eq!(strip_verbatim_prefix(r"\\?\D:\").as_deref(), Some(r"D:\"));
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\host\share\dir").as_deref(),
            Some(r"\\host\share\dir")
        );
        for other in [
            r"C:\Users\me",
            "/home/me",
            r"\\?\Volume{1234}\dir",
            r"\\?\pipe\name",
            r"\\server\share",
            "",
        ] {
            assert_eq!(strip_verbatim_prefix(other), None, "{other:?}");
        }
    }

    #[test]
    fn dirs_go_first_in_order_and_once() {
        let first = [PathBuf::from("rtk-home"), PathBuf::from("tools")];
        let old = std::env::join_paths(["a", "tools", "b", "rtk-home"]).unwrap();
        let joined = path_with_dirs_first(Some(&old), &first);
        let parts: Vec<_> = std::env::split_paths(&joined).collect();
        assert_eq!(
            parts,
            [
                PathBuf::from("rtk-home"),
                PathBuf::from("tools"),
                PathBuf::from("a"),
                PathBuf::from("b")
            ]
        );
        let alone = path_with_dirs_first(None, &first);
        assert_eq!(std::env::split_paths(&alone).collect::<Vec<_>>(), first);
    }

    #[test]
    fn a_posix_shell_brings_its_tools_directory() {
        let tools = Path::new("pf").join("Git").join("usr").join("bin");
        let mut machine = windows_machine(true);
        // no usr\bin next to bin\bash.exe: nothing to add
        assert_eq!(posix_tool_dirs(&pf_bash(), &machine), Vec::<PathBuf>::new());
        // with it, the tools directory is added (a directory exists when a file lives in it)
        machine.files.push(tools.join("ls.exe"));
        assert_eq!(posix_tool_dirs(&pf_bash(), &machine), vec![tools.clone()]);
        // a shell that sits in the tools directory itself (MSYS2, Git's usr\bin\bash.exe)
        assert_eq!(
            posix_tool_dirs(&tools.join("bash.exe"), &machine),
            vec![tools]
        );
        // any other layout: nothing
        assert!(posix_tool_dirs(Path::new("bash.exe"), &machine).is_empty());
        assert!(posix_tool_dirs(&Path::new("opt").join("zsh.exe"), &machine).is_empty());
    }

    #[test]
    fn posix_command_puts_the_tools_first_on_path() {
        let tools = vec![PathBuf::from("usr-bin")];
        let old = std::env::join_paths(["a", "b"]).unwrap();
        let command = posix_command(Path::new("bash.exe"), "ls", &tools, Some(&old));
        let path = command
            .get_envs()
            .find(|(key, _)| key.to_str() == Some("PATH"))
            .and_then(|(_, value)| value)
            .expect("PATH is set for the child");
        assert_eq!(
            std::env::split_paths(path).collect::<Vec<_>>(),
            [
                PathBuf::from("usr-bin"),
                PathBuf::from("a"),
                PathBuf::from("b")
            ]
        );
        // no tools directory: the environment is left alone
        let plain = posix_command(Path::new("bash.exe"), "ls", &[], Some(&old));
        assert_eq!(plain.get_envs().count(), 0);
    }

    #[test]
    fn base_command_for_the_allowlist() {
        // unchanged on Unix
        assert_eq!(base_command("git", false), "git");
        assert_eq!(base_command("/usr/bin/git", false), "git");
        assert_eq!(base_command("./Git.EXE", false), "Git.EXE");
        // Windows: directory, extension and case do not count
        assert_eq!(base_command("git", true), "git");
        assert_eq!(
            base_command("C:\\Program Files\\Git\\cmd\\GIT.EXE", true),
            "git"
        );
        assert_eq!(base_command("C:/tools/rtk.exe", true), "rtk");
        assert_eq!(base_command("dir", true), "dir");
        assert_eq!(base_command(".exe", true), "");
        assert_eq!(base_command("x", true), "x");
    }

    #[test]
    fn program_of_a_command_line_on_unix() {
        let program = |line| program_of(line, false);
        assert_eq!(program("git status"), Ok("git".to_string()));
        assert_eq!(
            program("  /usr/bin/git  status"),
            Ok("/usr/bin/git".to_string())
        );
        assert_eq!(program("'my tool' --x"), Ok("my tool".to_string()));
        assert_eq!(program("\"a b\" c"), Ok("a b".to_string()));
        // POSIX rules: an unquoted backslash escapes the next character
        assert_eq!(program("a\\ b c"), Ok("a b".to_string()));
        assert_eq!(
            program("echo \"unterminated"),
            Err("Failed to parse command: unmatched quotes".to_string())
        );
        assert_eq!(
            program(""),
            Err("Error: empty command after parsing".to_string())
        );
    }

    #[test]
    fn program_of_a_command_line_on_windows_keeps_backslashes() {
        let program = |line| program_of(line, true);
        assert_eq!(program("dir /b"), Ok("dir".to_string()));
        assert_eq!(program("  dir"), Ok("dir".to_string()));
        assert_eq!(
            program("C:\\Windows\\System32\\HOSTNAME.EXE /all"),
            Ok("C:\\Windows\\System32\\HOSTNAME.EXE".to_string())
        );
        assert_eq!(
            program("\"C:\\Program Files\\Git\\cmd\\git.exe\" status"),
            Ok("C:\\Program Files\\Git\\cmd\\git.exe".to_string())
        );
        assert_eq!(
            program("'C:/Program Files/Git/cmd/git.exe' status"),
            Ok("C:/Program Files/Git/cmd/git.exe".to_string())
        );
        // an unterminated quote is the shell's to complain about, not the allowlist's
        assert_eq!(
            program("\"C:\\tools\\x.exe"),
            Ok("C:\\tools\\x.exe".to_string())
        );
        assert_eq!(program("dir C:\\"), Ok("dir".to_string()));
        assert_eq!(
            program("\"\""),
            Err("Error: empty command after parsing".to_string())
        );
        assert_eq!(
            program("   "),
            Err("Error: empty command after parsing".to_string())
        );
        // and the pair the allowlist uses names the program, not the folder or the extension
        for line in [
            "C:\\Windows\\System32\\HOSTNAME.EXE",
            "\"C:\\Windows\\System32\\hostname.exe\" /all",
            "C:/Windows/System32/hostname.exe",
        ] {
            let base = base_command(&program(line).unwrap(), true);
            assert_eq!(base, "hostname", "{line}");
        }
    }

    #[test]
    fn rtk_beside_the_server_is_found() {
        let dir = std::env::temp_dir().join(format!("rtk-mcp-bundled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let server = dir.join("rtk-mcp.exe");
        assert_eq!(bundled_rtk_dir_of(&server), None);
        std::fs::write(dir.join("rtk.exe"), b"").unwrap();
        assert_eq!(bundled_rtk_dir_of(&server), Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plain_form_of_a_unix_path_is_itself() {
        #[cfg(not(windows))]
        assert_eq!(plain_path(PathBuf::from("/tmp/x")), PathBuf::from("/tmp/x"));
    }
}
