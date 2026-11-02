//! Which `rtk` this server drives.
//!
//! RTK used to be looked up once, by name, when the server started. That is too little in two
//! cases. A GUI application (Claude Desktop) starts its servers with a short PATH that often lacks
//! `~/.local/bin`, `~/.cargo/bin` and Homebrew's directory. And an RTK that [`crate::install`] puts
//! on the machine while the server runs has to be usable at once, without a restart.
//!
//! So RTK is looked for in a fixed list of places, remembered by absolute path, and the directory
//! it sits in is made visible to the commands the server runs: a rewritten command is the string
//! `rtk git status`, which the shell resolves by name.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::shell::{self, Machine, RealMachine};
use crate::{interpret_rewrite, Rewrite};

/// The file name of the RTK executable.
pub const EXE: &str = if cfg!(windows) { "rtk.exe" } else { "rtk" };

/// How long one probe (`rtk --version`, `rtk rewrite`) may take before a candidate is given up on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// RTK 0.49 is the first release with `rtk recall`.
pub const RECALL_SINCE: Version = Version(0, 49, 0);

/// How an RTK was found, for the status report and the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// `RTK_MCP_RTK_PATH` names it.
    Setting,
    /// It sits next to the server's own executable.
    Beside,
    /// It is on the PATH the server was started with.
    Path,
    /// It is in a directory RTK is usually installed to, though not on the server's PATH.
    Usual,
    /// `install_rtk` (or `RTK_MCP_AUTO_INSTALL`) installed it.
    Installed,
}

impl Origin {
    pub fn describe(self) -> &'static str {
        match self {
            Origin::Setting => "set by RTK_MCP_RTK_PATH",
            Origin::Beside => "next to the rtk-mcp executable",
            Origin::Path => "found on PATH",
            Origin::Usual => "found in a usual install directory",
            Origin::Installed => "installed by rtk-mcp",
        }
    }
}

/// An RTK that works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rtk {
    pub path: PathBuf,
    /// The first line of `rtk --version`, e.g. `rtk 1.1.0`.
    pub version: String,
    pub origin: Origin,
}

impl Rtk {
    /// Is this RTK at least `wanted`? An unreadable version counts as too old: the callers use
    /// this to keep from running a subcommand an old RTK would take for a program to execute.
    pub fn at_least(&self, wanted: Version) -> bool {
        parse_version(&self.version).is_some_and(|have| have >= wanted)
    }
}

/// `major.minor.patch` of an RTK release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version in a `rtk --version` line (`rtk 1.1.0`, `rtk 0.34.3`, `rtk 1.2.0-rc.1`).
pub fn parse_version(line: &str) -> Option<Version> {
    let number = line.split_whitespace().nth(1)?;
    let mut parts = number.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u32>().ok()
    });
    Some(Version(parts.next()??, parts.next()??, parts.next()??))
}

/// The RTK in use. One server runs per process, so this is process-wide state; it changes when
/// RTK is installed or found after the start.
static CURRENT: RwLock<Option<Arc<Rtk>>> = RwLock::new(None);

/// The RTK in use, if there is one.
pub fn current() -> Option<Arc<Rtk>> {
    CURRENT
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Use `rtk` from now on (`None`: there is none).
pub fn set_current(rtk: Option<Rtk>) {
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = rtk.map(Arc::new);
}

/// The user's home directory.
pub fn home_dir(machine: &dyn Machine) -> Option<PathBuf> {
    let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    machine
        .var(name)
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

/// Directories RTK usually lives in, which a GUI-started server's PATH often lacks.
fn usual_dirs(machine: &dyn Machine) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = crate::install::install_dir(machine).into_iter().collect();
    if let Some(home) = home_dir(machine) {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
        if cfg!(windows) {
            dirs.push(home.join("scoop").join("shims"));
        }
    }
    if cfg!(windows) {
        if let Some(local) = machine.var("LOCALAPPDATA").filter(|dir| !dir.is_empty()) {
            // where `winget install rtk-ai.rtk` links its command
            dirs.push(PathBuf::from(local).join("Microsoft/WinGet/Links"));
        }
    } else {
        for dir in [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/home/linuxbrew/.linuxbrew/bin",
        ] {
            dirs.push(PathBuf::from(dir));
        }
    }
    dirs
}

/// Every place an RTK executable is looked for, most preferred first.
///
/// 1. `RTK_MCP_RTK_PATH`, when the user names one.
/// 2. Next to the server's own executable (how the Windows installer lays the two out).
/// 3. The PATH the server was started with.
/// 4. The usual install directories (see [`usual_dirs`]).
pub fn candidates(machine: &dyn Machine, own_exe: Option<&Path>) -> Vec<(Origin, PathBuf)> {
    let mut list: Vec<(Origin, PathBuf)> = Vec::new();
    let mut add = |origin: Origin, path: PathBuf| {
        if !list.iter().any(|(_, seen)| *seen == path) {
            list.push((origin, path));
        }
    };
    if let Some(setting) = machine.var("RTK_MCP_RTK_PATH").filter(|v| !v.is_empty()) {
        add(Origin::Setting, PathBuf::from(setting));
    }
    if let Some(dir) = own_exe.and_then(Path::parent) {
        add(Origin::Beside, dir.join(EXE));
    }
    if let Some(path) = machine.var("PATH") {
        for dir in std::env::split_paths(&path).filter(|dir| !dir.as_os_str().is_empty()) {
            add(Origin::Path, dir.join(EXE));
        }
    }
    for dir in usual_dirs(machine) {
        add(Origin::Usual, dir.join(EXE));
    }
    list
}

/// What a search came up with.
#[derive(Debug, Default)]
pub struct Search {
    pub found: Option<Rtk>,
    /// Candidates that exist but are not a working RTK (another program called `rtk`, a damaged
    /// file, a wrong `RTK_MCP_RTK_PATH`), each with the reason, for the status report.
    pub skipped: Vec<String>,
}

/// The first candidate for which `probe` returns a version.
pub fn search_with(
    machine: &dyn Machine,
    own_exe: Option<&Path>,
    probe: &dyn Fn(&Path) -> Option<String>,
) -> Search {
    let mut skipped = Vec::new();
    for (origin, path) in candidates(machine, own_exe) {
        if !machine.is_file(&path) {
            if origin == Origin::Setting {
                skipped.push(format!("RTK_MCP_RTK_PATH={} is not a file", path.display()));
            }
            continue;
        }
        match probe(&path) {
            Some(version) => {
                return Search {
                    found: Some(Rtk {
                        path,
                        version,
                        origin,
                    }),
                    skipped,
                }
            }
            None => skipped.push(format!(
                "{} ({}) is not a working RTK",
                path.display(),
                origin.describe()
            )),
        }
    }
    Search {
        found: None,
        skipped,
    }
}

/// [`search_with`] on this machine.
pub fn search() -> Search {
    search_with(
        &RealMachine,
        std::env::current_exe().ok().as_deref(),
        &probe,
    )
}

/// The PATH the commands the server runs need so that `rtk` is the RTK in use, or `None` when the
/// PATH they get anyway already does that.
///
/// * The first `rtk` on the PATH is this one: nothing to change.
/// * Another `rtk` comes first (an older one, or a different program of that name): this RTK's
///   directory goes to the front, since it is the one that was chosen.
/// * There is no `rtk` on the PATH, as when a GUI application started the server: the directory
///   is added at the end, which leaves every other command resolving exactly as before.
pub fn path_for_children(
    rtk: &Rtk,
    path: Option<&OsStr>,
    machine: &dyn Machine,
) -> Option<OsString> {
    let dir = rtk.path.parent()?;
    let dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    match dirs.iter().find(|dir| machine.is_file(&dir.join(EXE))) {
        Some(first) if same_dir(first, dir) => None,
        Some(_) => Some(shell::path_with_dirs_first(path, &[dir.to_path_buf()])),
        None => {
            let mut all = dirs;
            all.push(dir.to_path_buf());
            std::env::join_paths(all).ok()
        }
    }
}

/// Are these the same directory? Case does not count on Windows.
fn same_dir(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// The `rtk --version` line when the program at `program` is a working RTK: it reports a version
/// that starts with `rtk ` (another tool of that name does not), and `rtk rewrite` — the routing
/// mechanism this server relies on — works. Stdin is null so a probe never touches the MCP
/// transport.
pub fn probe(program: &Path) -> Option<String> {
    let mut version_probe = std::process::Command::new(program);
    version_probe.arg("--version");
    let output = output_within(version_probe)?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !(version.starts_with("rtk ") && output.status.success()) {
        return None;
    }

    let mut rewrite_probe = std::process::Command::new(program);
    rewrite_probe.args(["rewrite", "--", "git status"]);
    if std::env::var_os("RTK_SUPPRESS_HOOK_WARNING").is_none() {
        rewrite_probe.env("RTK_SUPPRESS_HOOK_WARNING", "1");
    }
    let output = output_within(rewrite_probe)?;
    match interpret_rewrite(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
    ) {
        Rewrite::Passthrough => None,
        // A deny rule for `git status` still proves that `rtk rewrite` itself works.
        Rewrite::Rewritten(_) | Rewrite::Denied => Some(version),
    }
}

/// Run a probe and collect its stdout, giving up (and killing it) after [`PROBE_TIMEOUT`].
fn output_within(mut command: std::process::Command) -> Option<std::process::Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    hide_console(&mut command);
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
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
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }

        fn is_file(&self, path: &Path) -> bool {
            self.files.iter().any(|file| file == path)
        }

        fn is_dir(&self, path: &Path) -> bool {
            self.files
                .iter()
                .any(|file| file != path && file.starts_with(path))
        }
    }

    fn rtk_in(dir: &str) -> PathBuf {
        Path::new(dir).join(EXE)
    }

    fn home_var() -> &'static str {
        if cfg!(windows) {
            "USERPROFILE"
        } else {
            "HOME"
        }
    }

    fn joined(dirs: &[&str]) -> OsString {
        std::env::join_paths(dirs).unwrap()
    }

    fn working(version: &'static str) -> impl Fn(&Path) -> Option<String> {
        move |_| Some(version.to_string())
    }

    #[test]
    fn versions_are_read_from_the_version_line() {
        assert_eq!(parse_version("rtk 1.1.0"), Some(Version(1, 1, 0)));
        assert_eq!(parse_version("rtk 0.34.3"), Some(Version(0, 34, 3)));
        assert_eq!(parse_version("rtk 1.2.0-rc.1"), Some(Version(1, 2, 0)));
        assert_eq!(parse_version("rtk 0.49"), None);
        assert_eq!(parse_version("rtk"), None);
        assert_eq!(parse_version(""), None);
        assert!(Version(0, 49, 0) < Version(0, 49, 1));
        assert!(Version(0, 49, 1) < Version(1, 0, 0));
        assert!(Version(0, 9, 0) < Version(0, 34, 3));
        assert_eq!(Version(1, 1, 0).to_string(), "1.1.0");
    }

    #[test]
    fn features_are_gated_on_the_version() {
        let with = |version: &str| Rtk {
            path: PathBuf::from("rtk"),
            version: version.to_string(),
            origin: Origin::Path,
        };
        assert!(!with("rtk 0.34.3").at_least(RECALL_SINCE));
        assert!(with("rtk 0.49.0").at_least(RECALL_SINCE));
        assert!(with("rtk 1.1.0").at_least(RECALL_SINCE));
        assert!(
            !with("rtk ???").at_least(RECALL_SINCE),
            "an unreadable version is treated as too old"
        );
    }

    #[test]
    fn candidates_come_in_order_of_preference() {
        let machine = Fake {
            vars: vec![
                ("RTK_MCP_RTK_PATH", "custom/rtk".into()),
                ("PATH", joined(&["one", "two"])),
                (home_var(), "home".into()),
            ],
            files: vec![],
        };
        let list = candidates(&machine, Some(&Path::new("srv").join("rtk-mcp")));
        let origins: Vec<Origin> = list.iter().map(|(origin, _)| *origin).collect();
        assert_eq!(
            origins[..4],
            [Origin::Setting, Origin::Beside, Origin::Path, Origin::Path]
        );
        assert_eq!(list[0].1, PathBuf::from("custom/rtk"));
        assert_eq!(list[1].1, rtk_in("srv"));
        assert_eq!(list[2].1, rtk_in("one"));
        assert_eq!(list[3].1, rtk_in("two"));
        assert!(origins[4..].iter().all(|origin| *origin == Origin::Usual));
        let usual: Vec<&PathBuf> = list[4..].iter().map(|(_, path)| path).collect();
        let local_bin = Path::new("home").join(".local").join("bin").join(EXE);
        let cargo_bin = Path::new("home").join(".cargo").join("bin").join(EXE);
        assert!(usual.contains(&&local_bin), "{usual:?}");
        assert!(usual.contains(&&cargo_bin), "{usual:?}");
    }

    #[test]
    fn a_place_is_listed_once_at_its_first_rank() {
        let machine = Fake {
            vars: vec![
                ("PATH", joined(&["one", "one"])),
                (home_var(), "home".into()),
            ],
            files: vec![],
        };
        let list = candidates(&machine, Some(&Path::new("one").join("rtk-mcp")));
        // `one` is both beside the server and on the PATH: it counts as beside, and only once
        assert_eq!(list[0], (Origin::Beside, rtk_in("one")));
        assert_eq!(list.iter().filter(|(_, p)| *p == rtk_in("one")).count(), 1);
    }

    #[test]
    fn the_install_directory_is_a_usual_place() {
        let machine = Fake {
            vars: vec![("RTK_MCP_INSTALL_DIR", "somewhere".into())],
            files: vec![],
        };
        let list = candidates(&machine, None);
        assert!(
            list.contains(&(Origin::Usual, rtk_in("somewhere"))),
            "{list:?}"
        );
    }

    #[test]
    fn the_search_takes_the_first_candidate_that_works() {
        let machine = Fake {
            vars: vec![("PATH", joined(&["one", "two", "three"]))],
            files: vec![rtk_in("two"), rtk_in("three")],
        };
        let search = search_with(&machine, None, &working("rtk 1.1.0"));
        let found = search.found.expect("an RTK is found");
        assert_eq!(found.path, rtk_in("two"));
        assert_eq!(found.origin, Origin::Path);
        assert_eq!(found.version, "rtk 1.1.0");
        assert!(search.skipped.is_empty());
    }

    #[test]
    fn a_program_that_is_not_rtk_is_skipped_and_reported() {
        let machine = Fake {
            vars: vec![("PATH", joined(&["one", "two"]))],
            files: vec![rtk_in("one"), rtk_in("two")],
        };
        let only_two = |path: &Path| (path == rtk_in("two")).then(|| "rtk 0.49.0".to_string());
        let search = search_with(&machine, None, &only_two);
        assert_eq!(search.found.unwrap().path, rtk_in("two"));
        assert_eq!(search.skipped.len(), 1, "{:?}", search.skipped);
        assert!(search.skipped[0].contains("not a working RTK"));
    }

    #[test]
    fn a_wrong_setting_is_reported_and_the_search_goes_on() {
        let machine = Fake {
            vars: vec![
                ("RTK_MCP_RTK_PATH", "nope/rtk".into()),
                ("PATH", joined(&["one"])),
            ],
            files: vec![rtk_in("one")],
        };
        let search = search_with(&machine, None, &working("rtk 1.1.0"));
        assert_eq!(search.found.unwrap().path, rtk_in("one"));
        assert_eq!(search.skipped.len(), 1);
        assert!(search.skipped[0].contains("RTK_MCP_RTK_PATH"));
    }

    #[test]
    fn nothing_found_is_not_an_error() {
        let machine = Fake {
            vars: vec![("PATH", joined(&["one"]))],
            files: vec![],
        };
        let search = search_with(&machine, None, &working("rtk 1.1.0"));
        assert!(search.found.is_none());
        assert!(search.skipped.is_empty());
    }

    fn installed_in(dir: &str, origin: Origin) -> Rtk {
        Rtk {
            path: rtk_in(dir),
            version: "rtk 1.1.0".to_string(),
            origin,
        }
    }

    #[test]
    fn children_keep_their_path_when_rtk_already_resolves_to_the_one_in_use() {
        let machine = Fake {
            vars: vec![],
            files: vec![rtk_in("one")],
        };
        let path = joined(&["zero", "one", "two"]);
        let rtk = installed_in("one", Origin::Path);
        assert_eq!(path_for_children(&rtk, Some(&path), &machine), None);
    }

    #[test]
    fn children_get_the_chosen_rtk_in_front_when_another_comes_first() {
        let machine = Fake {
            vars: vec![],
            files: vec![rtk_in("old"), rtk_in("new")],
        };
        let path = joined(&["x", "old", "new", "y"]);
        let rtk = installed_in("new", Origin::Setting);
        let got = path_for_children(&rtk, Some(&path), &machine).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&got).collect();
        assert_eq!(
            dirs,
            ["new", "x", "old", "y"].map(PathBuf::from).to_vec(),
            "the chosen directory moves to the front, the rest keep their order"
        );
    }

    #[test]
    fn children_get_the_directory_at_the_end_when_the_path_has_no_rtk() {
        let machine = Fake {
            vars: vec![],
            files: vec![rtk_in("home-bin")],
        };
        let path = joined(&["usr-bin", "bin"]);
        let rtk = installed_in("home-bin", Origin::Usual);
        let got = path_for_children(&rtk, Some(&path), &machine).unwrap();
        let dirs: Vec<PathBuf> = std::env::split_paths(&got).collect();
        assert_eq!(
            dirs,
            ["usr-bin", "bin", "home-bin"].map(PathBuf::from).to_vec(),
            "nothing else on the PATH is reordered"
        );
        // and with no PATH at all
        let got = path_for_children(&rtk, None, &machine).unwrap();
        assert_eq!(std::env::split_paths(&got).count(), 1);
    }

    #[test]
    fn the_probe_rejects_a_program_that_is_not_rtk() {
        // `true`-like programs print no version; a missing file cannot be started at all
        assert_eq!(probe(Path::new("/definitely/not/here/rtk")), None);
        #[cfg(unix)]
        assert_eq!(probe(Path::new("/bin/true")), None);
    }
}
