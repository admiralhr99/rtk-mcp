//! Installing RTK from its GitHub release, and keeping it up to date, for the `install_rtk` tool
//! and `RTK_MCP_AUTO_INSTALL`.
//!
//! No installer script runs and no shell is involved. The release asset for this platform is
//! fetched over HTTPS with `curl` (on macOS, on Windows 10 and later, and on nearly every Linux),
//! compared with a SHA-256, unpacked in memory, and only the `rtk` executable is written, atomically.
//! A download whose hash is not the expected one is discarded unread.
//!
//! **Which release.** By default the latest one. GitHub's "latest release" page redirects to it, so
//! no API, token or JSON is involved, and the hashes are the ones in the `checksums.txt` published
//! with that release. That file comes from the same place as the archive: it catches a damaged or
//! half-uploaded download and a mirror that serves something else, but not a release that was
//! replaced at the source. For that case this build carries the hashes of one release ([`TAG`], in
//! `data/rtk-checksums-<tag>.txt`). That release is always checked against them, whatever GitHub
//! says, and `RTK_MCP_RTK_VERSION=pinned` restricts the installer to it.
//!
//! **An RTK that is already there** is upgraded in place when it is older than the release, and
//! only when it is a plain file in a directory the server can write to: the new executable is
//! written next to it, run once, and only then renamed over it. A symbolic link (a package
//! manager's), a read-only directory or a version that cannot be read leaves the file alone and
//! says why. An RTK as new as the release, or newer, is never touched, so nothing is downgraded.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::rtk::{self, Origin, Rtk, Version};
use crate::shell::{Machine, RealMachine};

/// One place for the release tag: it names the checksums file as well as the download.
macro_rules! tag {
    () => {
        "v1.1.0"
    };
}

/// The RTK release this build carries the hashes of: the one it falls back on when GitHub cannot
/// be asked for the latest, and the only one `RTK_MCP_RTK_VERSION=pinned` installs.
pub const TAG: &str = tag!();

/// The `checksums.txt` that release was published with, byte for byte. CI compares it with the
/// one on GitHub, so a re-uploaded asset is noticed.
const CHECKSUMS: &str = include_str!(concat!("../data/rtk-checksums-", tag!(), ".txt"));

const RELEASES: &str = "https://github.com/rtk-ai/rtk/releases/download";

/// GitHub redirects this page to the page of the newest release that is not a pre-release.
const LATEST_PAGE: &str = "https://github.com/rtk-ai/rtk/releases/latest";

/// The start of the page a release has; the redirect of [`LATEST_PAGE`] ends in the tag.
const TAG_PAGE: &str = "https://github.com/rtk-ai/rtk/releases/tag/";

/// The largest executable that is unpacked (RTK is about 15 MB), and the largest archive taken.
const MAX_EXE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;

/// A `checksums.txt` is a few hundred bytes; this much is plenty.
const MAX_CHECKSUMS_BYTES: usize = 1024 * 1024;

/// What a file being written is called until it is renamed into place.
const PART: &str = if cfg!(windows) { ".part.exe" } else { ".part" };

/// Where curl may throw away a body it has no use for.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The two builds of the same CLI that RTK ships from 1.0.0 on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    /// `rtk`, the standard build.
    Standard,
    /// `rtk-oss`, the Apache-2.0 build.
    Oss,
}

impl Edition {
    /// `RTK_MCP_RTK_EDITION`: `oss` selects the Apache-2.0 build, anything else the standard one.
    pub fn from_setting(value: Option<&str>) -> Edition {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("oss") => Edition::Oss,
            _ => Edition::Standard,
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Edition::Standard => "rtk",
            Edition::Oss => "rtk-oss",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Edition::Standard => {
                "Edition: standard (RTK_MCP_RTK_EDITION=oss selects the Apache-2.0 build)."
            }
            Edition::Oss => "Edition: oss (the Apache-2.0 build).",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    TarGz,
    Zip,
}

/// A release asset that holds the RTK executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub format: Format,
}

impl Asset {
    /// The file inside the archive that is RTK itself.
    fn member(&self) -> &'static str {
        match self.format {
            Format::TarGz => "rtk",
            Format::Zip => "rtk.exe",
        }
    }
}

/// The asset for an operating system and architecture, as `std::env::consts` names them.
pub fn asset(edition: Edition, os: &str, arch: &str) -> Option<Asset> {
    let (target, format) = match (os, arch) {
        ("linux", "x86_64") => ("x86_64-unknown-linux-musl", Format::TarGz),
        ("linux", "aarch64") => ("aarch64-unknown-linux-gnu", Format::TarGz),
        ("macos", "x86_64") => ("x86_64-apple-darwin", Format::TarGz),
        ("macos", "aarch64") => ("aarch64-apple-darwin", Format::TarGz),
        ("windows", "x86_64") => ("x86_64-pc-windows-msvc", Format::Zip),
        _ => return None,
    };
    let extension = match format {
        Format::TarGz => "tar.gz",
        Format::Zip => "zip",
    };
    Some(Asset {
        name: format!("{}-{target}.{extension}", edition.prefix()),
        format,
    })
}

/// The SHA-256 (lowercase hex) that a `checksums.txt` lists for the file `name`.
pub fn checksum_in(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        // `sha256sum` marks a binary-mode file with a `*` in front of its name
        let file = parts.next()?.trim_start_matches('*');
        let is_hash = hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit());
        (file == name && is_hash).then(|| hash.to_ascii_lowercase())
    })
}

/// The SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The executable inside a release archive. Only the bytes of the member named for this platform
/// are taken; nothing in the archive decides where anything is written.
pub fn extract(asset: &Asset, archive: &[u8]) -> Result<Vec<u8>, String> {
    let member = asset.member();
    match asset.format {
        Format::TarGz => {
            let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
            let entries = tar
                .entries()
                .map_err(|e| format!("{} is not a readable tar.gz: {e}", asset.name))?;
            for entry in entries {
                let entry = entry.map_err(|e| format!("{} is damaged: {e}", asset.name))?;
                if !entry.header().entry_type().is_file() {
                    continue;
                }
                let path = entry
                    .path()
                    .map_err(|e| format!("{} is damaged: {e}", asset.name))?;
                if path.file_name() == Some(OsStr::new(member)) {
                    return read_member(entry, member);
                }
            }
        }
        Format::Zip => {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
                .map_err(|e| format!("{} is not a readable zip: {e}", asset.name))?;
            for index in 0..zip.len() {
                let file = zip
                    .by_index(index)
                    .map_err(|e| format!("{} is damaged: {e}", asset.name))?;
                let is_member = file.is_file()
                    && file.name().is_ok_and(|name| {
                        Path::new(name.as_ref()).file_name() == Some(OsStr::new(member))
                    });
                if is_member {
                    return read_member(file, member);
                }
            }
        }
    }
    Err(format!("{} holds no {member}", asset.name))
}

/// Read an archive member, refusing one that unpacks to more than [`MAX_EXE_BYTES`].
fn read_member(reader: impl Read, member: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_EXE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot unpack {member}: {e}"))?;
    if bytes.len() as u64 > MAX_EXE_BYTES {
        return Err(format!(
            "{member} unpacks to more than {MAX_EXE_BYTES} bytes — refusing it"
        ));
    }
    if bytes.is_empty() {
        return Err(format!("{member} in the archive is empty"));
    }
    Ok(bytes)
}

/// Where a first `install_rtk` puts the executable: `RTK_MCP_INSTALL_DIR`, else `~/.local/bin`
/// (`%USERPROFILE%\.local\bin` on Windows), which is where the installers in this repository put it.
pub fn install_dir(machine: &dyn Machine) -> Option<PathBuf> {
    if let Some(dir) = machine
        .var("RTK_MCP_INSTALL_DIR")
        .filter(|dir| !dir.is_empty())
    {
        return Some(PathBuf::from(dir));
    }
    Some(rtk::home_dir(machine)?.join(".local").join("bin"))
}

/// One RTK release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub version: Version,
}

impl Release {
    /// The release a tag like `v1.1.0` names. Only `v<major>.<minor>.<patch>` is a release to
    /// install: the tag goes into URLs, and a pre-release suffix means it is not the latest anyway.
    pub fn from_tag(tag: &str) -> Option<Release> {
        let number = tag.strip_prefix('v')?;
        let parts: Vec<&str> = number.split('.').collect();
        let numeric = |part: &&str| {
            !part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit())
        };
        if parts.len() != 3 || !parts.iter().all(numeric) {
            return None;
        }
        let version = rtk::parse_version(&format!("rtk {number}"))?;
        Some(Release {
            tag: tag.to_string(),
            version,
        })
    }

    /// The release this build carries the hashes of.
    pub fn pinned() -> Release {
        Release::from_tag(TAG).expect("TAG is a release tag")
    }

    fn is_pinned(&self) -> bool {
        self.tag == TAG
    }
}

/// The release that a redirect from GitHub's "latest release" page leads to.
pub fn release_from_redirect(location: &str) -> Option<Release> {
    Release::from_tag(location.trim().strip_prefix(TAG_PAGE)?)
}

/// Which release `install_rtk` goes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// The latest release on GitHub (the default).
    Latest,
    /// The release this build of rtk-mcp carries the hashes of ([`TAG`]).
    Pinned,
}

impl Channel {
    /// `RTK_MCP_RTK_VERSION`: `latest` (or nothing) or `pinned`. Anything else is an error, so that
    /// a typo cannot quietly select the other one.
    pub fn from_setting(value: Option<&str>) -> Result<Channel, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("latest") => Ok(Channel::Latest),
            Some("pinned") => Ok(Channel::Pinned),
            Some(other) => Err(format!(
                "RTK_MCP_RTK_VERSION={other} is not understood; use `latest` (the default) or \
                 `pinned`"
            )),
        }
    }

    pub fn current() -> Result<Channel, String> {
        Channel::from_setting(std::env::var("RTK_MCP_RTK_VERSION").ok().as_deref())
    }
}

/// Where the expected hash of a download comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashSource {
    /// Compiled into rtk-mcp.
    Pinned,
    /// The `checksums.txt` published with the release.
    Published,
}

/// The edition and asset to fetch on this platform.
fn wanted_asset(machine: &dyn Machine, os: &str, arch: &str) -> Result<(Edition, Asset), String> {
    let edition = Edition::from_setting(
        machine
            .var("RTK_MCP_RTK_EDITION")
            .as_deref()
            .and_then(OsStr::to_str),
    );
    let asset = asset(edition, os, arch).ok_or_else(|| {
        format!(
            "there is no prebuilt RTK for {os} on {arch}; build it with \
             `cargo install --git https://github.com/rtk-ai/rtk`"
        )
    })?;
    Ok((edition, asset))
}

/// Everything settled before anything is downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub edition: Edition,
    pub asset: Asset,
    pub release: Release,
    pub url: String,
    /// The SHA-256 the asset must have.
    pub sha256: String,
    pub source: HashSource,
    /// The file that is written: the RTK being upgraded, or `rtk` in the install directory.
    pub target: PathBuf,
}

/// The plan for `release`. `published` is that release's `checksums.txt`, which is needed unless
/// it is the pinned release (whose hashes are compiled in and are used whatever `published` says);
/// `replacing` is the RTK being upgraded.
pub fn plan_for(
    machine: &dyn Machine,
    os: &str,
    arch: &str,
    release: &Release,
    published: Option<&str>,
    replacing: Option<&Path>,
) -> Result<Plan, String> {
    let (edition, asset) = wanted_asset(machine, os, arch)?;
    let (text, source) = if release.is_pinned() {
        (CHECKSUMS, HashSource::Pinned)
    } else {
        let text =
            published.ok_or_else(|| format!("there is no checksums.txt for {}", release.tag))?;
        (text, HashSource::Published)
    };
    let sha256 = checksum_in(text, &asset.name).ok_or_else(|| match source {
        HashSource::Pinned => format!("rtk-mcp has no pinned checksum for {}", asset.name),
        HashSource::Published => {
            format!(
                "the checksums.txt of {} lists no {}",
                release.tag, asset.name
            )
        }
    })?;
    let target = match replacing {
        Some(path) => path.to_path_buf(),
        None => install_dir(machine)
            .ok_or_else(|| {
                "cannot tell where to install RTK: HOME is not set; set RTK_MCP_INSTALL_DIR"
                    .to_string()
            })?
            .join(rtk::EXE),
    };
    let url = format!("{RELEASES}/{}/{}", release.tag, asset.name);
    Ok(Plan {
        edition,
        asset,
        release: release.clone(),
        url,
        sha256,
        source,
        target,
    })
}

/// The plan for the pinned release into the install directory: what `get_rtk_status` describes.
pub fn plan(machine: &dyn Machine, os: &str, arch: &str) -> Result<Plan, String> {
    plan_for(machine, os, arch, &Release::pinned(), None, None)
}

/// What to do about the RTK that is in use (if any) given the release to go for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision<'a> {
    /// There is none: install one.
    Install,
    /// It is older than the release: replace it.
    Upgrade(&'a Rtk),
    /// It is as new as the release, or newer: leave it.
    Current(&'a Rtk),
    /// Its version cannot be read, so it cannot be compared: leave it.
    Unreadable(&'a Rtk),
}

pub fn decide(installed: Option<&Rtk>, target: Version) -> Decision<'_> {
    match installed {
        None => Decision::Install,
        Some(rtk) => match rtk::parse_version(&rtk.version) {
            None => Decision::Unreadable(rtk),
            Some(have) if have < target => Decision::Upgrade(rtk),
            Some(_) => Decision::Current(rtk),
        },
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(|| "rtk".into(), |name| name.to_string_lossy().into_owned())
}

/// The directory a file is in, `.` for a bare file name.
fn directory_of(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// How the package manager behind a symbolic link updates RTK, judging by where the link and its
/// target are.
fn manager_hint(link: &Path, behind: Option<&Path>) -> &'static str {
    let text = format!(
        "{} {}",
        link.display(),
        behind
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    )
    .to_ascii_lowercase();
    if text.contains("cellar") || text.contains("homebrew") || text.contains("linuxbrew") {
        "update it with `brew upgrade rtk`"
    } else if text.contains("winget") {
        "update it with `winget upgrade rtk-ai.rtk`"
    } else if text.contains("scoop") {
        "update it with `scoop update rtk`"
    } else {
        "update it the way you installed it"
    }
}

/// Can the RTK at `path` be replaced in place? Not when it is a symbolic link (most likely a
/// package manager's, and the file behind it is the package manager's to replace), not when it is
/// not a regular file, and not when its directory cannot be written to. The reason comes back as
/// a clause that ends the sentence "it was left alone: ...".
pub fn replaceable(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("{} cannot be looked at ({e})", path.display()))?;
    if meta.file_type().is_symlink() {
        let behind = std::fs::read_link(path).ok();
        let to = behind
            .as_ref()
            .map(|target| format!(" to {}", target.display()))
            .unwrap_or_default();
        return Err(format!(
            "{} is a symbolic link{to}, most likely a package manager's, and the file behind it \
             is the package manager's to replace: {}",
            path.display(),
            manager_hint(path, behind.as_deref())
        ));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    // Scoop's shims directory holds a small launcher (`rtk.exe`) and a `rtk.shim` that names the
    // real file; replacing the launcher would cut Scoop's copy off.
    let shim = path.with_extension("shim");
    if shim.is_file() {
        return Err(format!(
            "{} is a Scoop shim (there is a {} beside it) and the file it launches is Scoop's to \
             replace: update it with `scoop update rtk`",
            path.display(),
            file_name(&shim)
        ));
    }
    let dir = directory_of(path);
    let probe = dir.join(format!(".{}.{}.check", file_name(path), std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(format!(
            "rtk-mcp cannot write in {} ({e}): update RTK the way you installed it, or with \
             administrator rights",
            dir.display()
        )),
    }
}

/// Check a downloaded archive against the expected hash, unpack the executable and put it at the
/// plan's target. Returns the version line of the RTK that is now there. Nothing is written unless
/// the hash matches.
///
/// `probe` runs the new executable once before it replaces anything; `not_older_than` is the
/// version of the RTK being replaced, which the new one must not be older than.
pub fn finish(
    plan: &Plan,
    archive: &[u8],
    not_older_than: Option<Version>,
    probe: &dyn Fn(&Path) -> Option<String>,
) -> Result<String, String> {
    let actual = sha256_hex(archive);
    if actual != plan.sha256 {
        let whose = match plan.source {
            HashSource::Pinned => "pinned in rtk-mcp".to_string(),
            HashSource::Published => format!("published with {}", plan.release.tag),
        };
        return Err(format!(
            "the download of {} does not match the checksum {whose} (expected {}, got {actual}); \
             nothing was installed",
            plan.asset.name, plan.sha256
        ));
    }
    let exe = extract(&plan.asset, archive)?;
    place(&plan.target, &exe, not_older_than, probe)
}

/// Write `bytes` as the RTK executable at `target`: to a temporary file in the same directory
/// first, so that the last step is an atomic rename and a reader never sees half a file. The new
/// executable is run before that rename, in the place it is going to live, so an RTK that does not
/// work on this machine never replaces one that does.
fn place(
    target: &Path,
    bytes: &[u8],
    not_older_than: Option<Version>,
    probe: &dyn Fn(&Path) -> Option<String>,
) -> Result<String, String> {
    let dir = directory_of(target);
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(windows)]
    remove_stale_aside(target);
    let staged = dir.join(format!(
        ".{}.{}{PART}",
        file_name(target),
        std::process::id()
    ));
    let discard = |reason: String| {
        let _ = std::fs::remove_file(&staged);
        Err(reason)
    };
    let write = || -> std::io::Result<()> {
        std::fs::write(&staged, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        return discard(format!("cannot write {}: {e}", target.display()));
    }
    let Some(version) = probe(&staged) else {
        return discard(format!(
            "the downloaded RTK does not run on this machine, so {} was left as it was; install \
             RTK by hand (https://github.com/rtk-ai/rtk)",
            target.display()
        ));
    };
    if let (Some(floor), Some(got)) = (not_older_than, rtk::parse_version(&version)) {
        if got < floor {
            return discard(format!(
                "the downloaded RTK reports {got}, which is older than the {floor} in use, so {} \
                 was left as it was",
                target.display()
            ));
        }
    }
    if let Err(e) = replace_file(&staged, target) {
        return discard(format!("cannot write {}: {e}", target.display()));
    }
    Ok(version)
}

/// Rename `staged` over `target`. Windows will not overwrite an executable that is running, but it
/// lets one be renamed, so there the old file is moved aside and deleted if it can be.
fn replace_file(staged: &Path, target: &Path) -> std::io::Result<()> {
    let first = match std::fs::rename(staged, target) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    #[cfg(windows)]
    {
        if target.exists() {
            let aside = aside_name(target);
            if std::fs::rename(target, &aside).is_ok() {
                return match std::fs::rename(staged, target) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&aside);
                        Ok(())
                    }
                    Err(e) => {
                        let _ = std::fs::rename(&aside, target);
                        Err(e)
                    }
                };
            }
        }
    }
    Err(first)
}

/// Where Windows has the old executable wait until nothing runs it any more.
#[cfg(windows)]
fn aside_name(target: &Path) -> PathBuf {
    directory_of(target).join(format!(".{}.old-{}", file_name(target), std::process::id()))
}

/// A moved-aside executable that was still running when it could not be deleted is deleted by the
/// next install.
#[cfg(windows)]
fn remove_stale_aside(target: &Path) {
    let prefix = format!(".{}.old-", file_name(target));
    if let Ok(entries) = std::fs::read_dir(directory_of(target)) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// `RTK_MCP_NO_INSTALL=1`: this server never downloads anything, and has no `install_rtk` tool.
pub fn disabled() -> bool {
    flag("RTK_MCP_NO_INSTALL")
}

/// `RTK_MCP_AUTO_INSTALL=1`: at startup, in the background, install RTK when none is found and
/// upgrade the one that is found when it is older than the latest release.
pub fn auto_enabled() -> bool {
    flag("RTK_MCP_AUTO_INSTALL") && !disabled()
}

/// A `curl` for `url`: TLS 1.2 or later, https only (also across redirects, which GitHub uses to
/// hand an asset to its storage), and limits on size and time. The proxy settings of the
/// environment apply as they do for any curl. `extra` are further options.
fn curl(url: &str, max_seconds: &str, max_bytes: usize, extra: &[&str]) -> Command {
    let max_bytes = max_bytes.to_string();
    let mut command = Command::new(curl_program());
    command
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "20",
            "--max-time",
            max_seconds,
            "--max-filesize",
            max_bytes.as_str(),
            "--user-agent",
            concat!("rtk-mcp/", env!("CARGO_PKG_VERSION")),
        ])
        .args(extra)
        .arg(url)
        .stdin(Stdio::null()) // never the MCP transport
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(crate::shell::CREATE_NO_WINDOW);
    command
}

fn curl_program() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

/// Run a command made by [`curl`] and return what it printed.
async fn run_curl(mut command: Command, url: &str) -> Result<Vec<u8>, String> {
    let output = command.output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "cannot download: `{}` is not installed. Install RTK by hand instead \
                 ({url}), or install curl",
                curl_program()
            )
        } else {
            format!("cannot run {}: {e}", curl_program())
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("downloading {url} failed: {}", reason.trim()));
    }
    Ok(output.stdout)
}

/// The release asset itself.
async fn download(url: &str) -> Result<Vec<u8>, String> {
    let bytes = run_curl(curl(url, "150", MAX_ARCHIVE_BYTES, &["--location"]), url).await?;
    if bytes.len() > MAX_ARCHIVE_BYTES {
        return Err(format!("{url} is larger than {MAX_ARCHIVE_BYTES} bytes"));
    }
    Ok(bytes)
}

/// A small text file (a `checksums.txt`).
async fn fetch_text(url: &str) -> Result<String, String> {
    let bytes = run_curl(curl(url, "30", MAX_CHECKSUMS_BYTES, &["--location"]), url).await?;
    if bytes.len() > MAX_CHECKSUMS_BYTES {
        return Err(format!("{url} is larger than {MAX_CHECKSUMS_BYTES} bytes"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Where `url` redirects to, without following it: the answer is the `Location` of the first
/// response.
async fn redirect_of(url: &str) -> Result<String, String> {
    let command = curl(
        url,
        "30",
        64 * 1024,
        &["--output", NULL_DEVICE, "--write-out", "%{redirect_url}"],
    );
    let bytes = run_curl(command, url).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_string())
}

/// The release GitHub calls the latest, and its `checksums.txt` unless it is the pinned release
/// (whose hashes are compiled in). A release that lists no asset for this platform is not one
/// to go for, so that is an error too.
async fn latest(asset: &Asset) -> Result<(Release, Option<String>), String> {
    let location = redirect_of(LATEST_PAGE).await?;
    let release = release_from_redirect(&location).ok_or_else(|| {
        if location.is_empty() {
            "GitHub's latest-release page did not lead to a release".to_string()
        } else {
            format!("GitHub's latest-release page led to {location}, which is not a release tag")
        }
    })?;
    if release.is_pinned() {
        return Ok((release, None));
    }
    let text = fetch_text(&format!("{RELEASES}/{}/checksums.txt", release.tag)).await?;
    if checksum_in(&text, &asset.name).is_none() {
        return Err(format!(
            "the checksums.txt of {} lists no {} (is the release still being uploaded?)",
            release.tag, asset.name
        ));
    }
    Ok((release, Some(text)))
}

/// The release to go for, where its hashes are, and what to call it in messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub release: Release,
    /// The release's `checksums.txt`; none for the pinned release.
    pub published: Option<String>,
    /// What this release is: "the latest release", or why it is something else.
    pub label: String,
}

async fn choose(channel: Channel, asset: &Asset) -> Target {
    match channel {
        Channel::Pinned => Target {
            release: Release::pinned(),
            published: None,
            label: "the release RTK_MCP_RTK_VERSION=pinned selects".to_string(),
        },
        Channel::Latest => match latest(asset).await {
            Ok((release, published)) => Target {
                release,
                published,
                label: "the latest release".to_string(),
            },
            Err(reason) => {
                tracing::warn!("cannot tell which RTK release is the latest: {reason}");
                Target {
                    release: Release::pinned(),
                    published: None,
                    label: format!(
                        "the release this rtk-mcp was built with — GitHub could not be asked \
                         for the latest ({reason})"
                    ),
                }
            }
        },
    }
}

/// What an install did.
#[derive(Debug, Clone)]
pub struct Done {
    pub rtk: Rtk,
    pub plan: Plan,
    pub label: String,
}

/// What `install_rtk` did, or did not do.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// There was no RTK; now there is.
    Installed(Done),
    /// An older RTK was replaced by the release.
    Upgraded { from: String, done: Done },
    /// The RTK in use is as new as the release, or newer.
    Current { rtk: Rtk, target: Target },
    /// The RTK in use is older, and was left alone for the reason given.
    Left {
        rtk: Rtk,
        target: Target,
        why: String,
    },
}

/// `1.1.0` out of `rtk 1.1.0`.
fn number(version_line: &str) -> &str {
    version_line.strip_prefix("rtk ").unwrap_or(version_line)
}

impl Done {
    fn provenance(&self) -> String {
        let whose = match self.plan.source {
            HashSource::Pinned => "the one pinned in rtk-mcp".to_string(),
            HashSource::Published => format!(
                "the one in the checksums.txt published with {}",
                self.plan.release.tag
            ),
        };
        format!(
            "{}, {}; {}, whose SHA-256, {}, is {whose}",
            self.plan.release.tag, self.label, self.plan.asset.name, self.plan.sha256
        )
    }
}

impl Outcome {
    /// The answer of the `install_rtk` tool. An RTK that needed upgrading and was not is an
    /// error, so that the model does not take the job for done.
    pub fn report(&self) -> Result<String, String> {
        match self {
            Outcome::Installed(done) => Ok(format!(
                "Installed {} at {} ({}). run_command uses it from now on — no restart needed. \
                 The directory was not added to your shell's PATH. With RTK present, commands are \
                 checked by RTK's own rewrite and the deny rules in your Claude settings instead \
                 of the built-in allowlist. {}",
                done.rtk.version,
                done.rtk.path.display(),
                done.provenance(),
                done.plan.edition.describe()
            )),
            Outcome::Upgraded { from, done } => Ok(format!(
                "Upgraded RTK from {} to {} at {} ({}). The old file was replaced only after the \
                 new one had run successfully, and run_command uses the new RTK from the next \
                 command on — no restart needed. rtk-mcp does not touch RTK's settings or \
                 history. {}",
                number(from),
                number(&done.rtk.version),
                done.rtk.path.display(),
                done.provenance(),
                done.plan.edition.describe()
            )),
            Outcome::Current { rtk, target } => Ok(format!(
                "RTK is already installed and up to date: {} at {} ({}). It is not older than {}, \
                 {}. Nothing to install.",
                rtk.version,
                rtk.path.display(),
                rtk.origin.describe(),
                target.release.tag,
                target.label
            )),
            Outcome::Left { rtk, target, why } => Err(format!(
                "RTK was not upgraded. {} at {} is older than {} ({}), and it was left alone: \
                 {why}.",
                rtk.version,
                rtk.path.display(),
                target.release.tag,
                target.label
            )),
        }
    }

    /// One line for the log.
    pub fn log_line(&self) -> String {
        match self {
            Outcome::Installed(done) => format!(
                "installed {} at {} ({})",
                done.rtk.version,
                done.rtk.path.display(),
                done.plan.asset.name
            ),
            Outcome::Upgraded { from, done } => format!(
                "upgraded {} to {} at {} ({})",
                from,
                done.rtk.version,
                done.rtk.path.display(),
                done.plan.asset.name
            ),
            Outcome::Current { rtk, target } => format!(
                "{} at {} is up to date (not older than {}, {})",
                rtk.version,
                rtk.path.display(),
                target.release.tag,
                target.label
            ),
            Outcome::Left { rtk, target, why } => format!(
                "{} at {} is older than {} and was left alone: {why}",
                rtk.version,
                rtk.path.display(),
                target.release.tag
            ),
        }
    }
}

/// Set while an install runs, so a second call (the model's, and the automatic one at startup)
/// does not race the first over the same file.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Holds [`RUNNING`]; lets go when dropped, which also covers a call that is cancelled halfway.
struct Running;

impl Running {
    fn take() -> Option<Running> {
        (!RUNNING.swap(true, Ordering::SeqCst)).then_some(Running)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Install RTK, or upgrade the RTK in use to the release, and make the result the RTK in use.
pub async fn run() -> Result<Outcome, String> {
    if disabled() {
        return Err("installing RTK is turned off here (RTK_MCP_NO_INSTALL is set)".to_string());
    }
    let Some(_running) = Running::take() else {
        return Err("another install of RTK is already running; try again in a minute".to_string());
    };
    ensure().await
}

async fn ensure() -> Result<Outcome, String> {
    let channel = Channel::current()?;
    let machine = RealMachine;
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let (_, asset) = wanted_asset(&machine, os, arch)?;
    let installed = rtk::current();
    let target = choose(channel, &asset).await;

    let previous: Option<Rtk> = match decide(installed.as_deref(), target.release.version) {
        Decision::Current(rtk) => {
            return Ok(Outcome::Current {
                rtk: rtk.clone(),
                target,
            })
        }
        Decision::Unreadable(rtk) => {
            let why = format!("its version, `{}`, cannot be compared", rtk.version);
            return Ok(Outcome::Left {
                rtk: rtk.clone(),
                target,
                why,
            });
        }
        Decision::Upgrade(rtk) => {
            if let Err(why) = replaceable(&rtk.path) {
                return Ok(Outcome::Left {
                    rtk: rtk.clone(),
                    target,
                    why,
                });
            }
            Some(rtk.clone())
        }
        Decision::Install => None,
    };

    let plan = plan_for(
        &machine,
        os,
        arch,
        &target.release,
        target.published.as_deref(),
        previous.as_ref().map(|rtk| rtk.path.as_path()),
    )?;
    tracing::info!("downloading {}", plan.url);
    let archive = download(&plan.url).await?;
    let floor = previous
        .as_ref()
        .and_then(|rtk| rtk::parse_version(&rtk.version));
    let for_blocking = plan.clone();
    let version =
        tokio::task::spawn_blocking(move || finish(&for_blocking, &archive, floor, &rtk::probe))
            .await
            .map_err(|e| format!("the install task failed: {e}"))??;

    let now = Rtk {
        path: plan.target.clone(),
        version,
        origin: previous
            .as_ref()
            .map_or(Origin::Installed, |rtk| rtk.origin),
    };
    rtk::set_current(Some(now.clone()));
    let done = Done {
        rtk: now,
        plan,
        label: target.label,
    };
    Ok(match previous {
        Some(old) => Outcome::Upgraded {
            from: old.version,
            done,
        },
        None => Outcome::Installed(done),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::io::Write;

    struct Fake(Vec<(&'static str, OsString)>);

    impl Machine for Fake {
        fn var(&self, name: &str) -> Option<OsString> {
            self.0
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
        fn is_file(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
    }

    fn home_var() -> &'static str {
        if cfg!(windows) {
            "USERPROFILE"
        } else {
            "HOME"
        }
    }

    fn gzip(tar: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(tar).unwrap();
        gz.finish().unwrap()
    }

    /// A `.tar.gz` holding the given (path, bytes) files, the way a release archive does.
    fn tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        gzip(&builder.into_inner().unwrap())
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (path, bytes) in files {
            writer.start_file(*path, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn tar_asset() -> Asset {
        asset(Edition::Standard, "linux", "x86_64").unwrap()
    }

    fn zip_asset() -> Asset {
        asset(Edition::Standard, "windows", "x86_64").unwrap()
    }

    #[test]
    fn every_supported_platform_has_an_asset_with_a_pinned_checksum() {
        let platforms = [
            ("linux", "x86_64", "x86_64-unknown-linux-musl.tar.gz"),
            ("linux", "aarch64", "aarch64-unknown-linux-gnu.tar.gz"),
            ("macos", "x86_64", "x86_64-apple-darwin.tar.gz"),
            ("macos", "aarch64", "aarch64-apple-darwin.tar.gz"),
            ("windows", "x86_64", "x86_64-pc-windows-msvc.zip"),
        ];
        for (os, arch, suffix) in platforms {
            for (edition, prefix) in [(Edition::Standard, "rtk-"), (Edition::Oss, "rtk-oss-")] {
                let found = asset(edition, os, arch).unwrap_or_else(|| panic!("{os} {arch}"));
                assert_eq!(found.name, format!("{prefix}{suffix}"));
                let hash = checksum_in(CHECKSUMS, &found.name)
                    .unwrap_or_else(|| panic!("no pinned checksum for {}", found.name));
                assert_eq!(hash.len(), 64, "{}", found.name);
            }
        }
    }

    #[test]
    fn other_platforms_have_no_asset() {
        for (os, arch) in [
            ("linux", "x86"),
            ("linux", "riscv64"),
            ("freebsd", "x86_64"),
            ("windows", "aarch64"),
        ] {
            assert_eq!(asset(Edition::Standard, os, arch), None, "{os} {arch}");
        }
    }

    #[test]
    fn the_pinned_file_is_the_one_for_the_pinned_tag() {
        assert!(TAG.starts_with('v'));
        assert_eq!(Release::pinned().tag, TAG);
        // Each line is `<sha256>  <file>`; a hash listed twice for different files is fine
        // (the .deb copies), a file listed twice is not.
        let mut names: Vec<&str> = CHECKSUMS
            .lines()
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "a file is listed twice");
    }

    #[test]
    fn checksums_are_read_whatever_the_separator() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        let text = format!("{hash}  one.tar.gz\n{other} *two.zip\n{hash}\nnot a line\n");
        assert_eq!(checksum_in(&text, "one.tar.gz"), Some(hash));
        assert_eq!(checksum_in(&text, "two.zip"), Some(other));
        assert_eq!(checksum_in(&text, "three"), None);
        // a short or non-hex "hash" is not accepted
        assert_eq!(checksum_in("abc  x.zip", "x.zip"), None);
        assert_eq!(
            checksum_in(&format!("{}  x.zip", "g".repeat(64)), "x.zip"),
            None
        );
    }

    #[test]
    fn sha256_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_edition_comes_from_the_setting() {
        assert_eq!(Edition::from_setting(None), Edition::Standard);
        assert_eq!(Edition::from_setting(Some("oss")), Edition::Oss);
        assert_eq!(Edition::from_setting(Some(" OSS ")), Edition::Oss);
        assert_eq!(Edition::from_setting(Some("full")), Edition::Standard);
        assert_eq!(Edition::from_setting(Some("")), Edition::Standard);
    }

    #[test]
    fn the_install_directory_is_the_setting_or_local_bin() {
        let setting = Fake(vec![
            ("RTK_MCP_INSTALL_DIR", "custom".into()),
            (home_var(), "home".into()),
        ]);
        assert_eq!(install_dir(&setting), Some(PathBuf::from("custom")));
        let default = Fake(vec![(home_var(), "home".into())]);
        assert_eq!(
            install_dir(&default),
            Some(Path::new("home").join(".local").join("bin"))
        );
        let empty = Fake(vec![("RTK_MCP_INSTALL_DIR", "".into())]);
        assert_eq!(install_dir(&empty), None);
        assert_eq!(install_dir(&Fake(vec![])), None);
    }

    #[test]
    fn only_a_plain_version_tag_is_a_release() {
        let release = Release::from_tag("v1.1.0").unwrap();
        assert_eq!(release.tag, "v1.1.0");
        assert_eq!(release.version, Version(1, 1, 0));
        assert_eq!(
            Release::from_tag("v10.20.300").unwrap().version,
            Version(10, 20, 300)
        );
        for bad in [
            "",
            "v",
            "1.1.0",
            "v1.1",
            "v1.1.0.1",
            "v1.1.0-rc.1",
            "v1.1.0+build",
            "v1.x.0",
            "v1..0",
            "v1.1.0/",
            "v1.1.0/../x",
            "v1.1.0?x=1",
            "v-1.1.0",
            "v1234567890.1.1",
            " v1.1.0",
        ] {
            assert_eq!(Release::from_tag(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_latest_release_is_read_from_the_redirect() {
        let latest = |location: &str| release_from_redirect(location).map(|r| r.tag);
        assert_eq!(
            latest("https://github.com/rtk-ai/rtk/releases/tag/v1.2.3"),
            Some("v1.2.3".to_string())
        );
        assert_eq!(
            latest("  https://github.com/rtk-ai/rtk/releases/tag/v1.2.3\n"),
            Some("v1.2.3".to_string())
        );
        for bad in [
            "",
            "https://github.com/rtk-ai/rtk/releases/latest",
            "https://github.com/rtk-ai/rtk/releases/tag/v1.2.3-rc.1",
            "https://github.com/rtk-ai/rtk/releases/tag/nightly",
            "https://github.com/rtk-ai/rtk/releases/tag/v1.2.3/extra",
            "https://github.com/rtk-ai/rtk/releases/tag/v1.2.3?x=1",
            "http://github.com/rtk-ai/rtk/releases/tag/v1.2.3",
            "https://github.com/someone-else/rtk/releases/tag/v1.2.3",
            "https://evil.example/rtk-ai/rtk/releases/tag/v1.2.3",
            "https://github.com.evil.example/rtk-ai/rtk/releases/tag/v1.2.3",
        ] {
            assert_eq!(latest(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_channel_comes_from_the_setting() {
        assert_eq!(Channel::from_setting(None), Ok(Channel::Latest));
        assert_eq!(Channel::from_setting(Some("")), Ok(Channel::Latest));
        assert_eq!(Channel::from_setting(Some("latest")), Ok(Channel::Latest));
        assert_eq!(Channel::from_setting(Some(" LATEST ")), Ok(Channel::Latest));
        assert_eq!(Channel::from_setting(Some("pinned")), Ok(Channel::Pinned));
        assert_eq!(Channel::from_setting(Some("Pinned")), Ok(Channel::Pinned));
        // a version number is not a channel, and a typo does not quietly pick one
        for bad in ["1.1.0", "v1.1.0", "pinnd", "stable"] {
            let err = Channel::from_setting(Some(bad)).unwrap_err();
            assert!(err.contains("RTK_MCP_RTK_VERSION"), "{err}");
            assert!(err.contains("latest") && err.contains("pinned"), "{err}");
        }
    }

    fn rtk_of(version: &str) -> Rtk {
        Rtk {
            path: PathBuf::from("/x/rtk"),
            version: version.to_string(),
            origin: Origin::Path,
        }
    }

    #[test]
    fn what_to_do_depends_on_the_installed_version() {
        let target = Version(1, 1, 0);
        assert_eq!(decide(None, target), Decision::Install);
        let old = rtk_of("rtk 0.49.0");
        assert_eq!(decide(Some(&old), target), Decision::Upgrade(&old));
        let older_patch = rtk_of("rtk 1.0.9");
        assert_eq!(
            decide(Some(&older_patch), target),
            Decision::Upgrade(&older_patch)
        );
        let same = rtk_of("rtk 1.1.0");
        assert_eq!(decide(Some(&same), target), Decision::Current(&same));
        let newer = rtk_of("rtk 1.2.0");
        assert_eq!(decide(Some(&newer), target), Decision::Current(&newer));
        let much_newer = rtk_of("rtk 2.0.0");
        assert_eq!(
            decide(Some(&much_newer), target),
            Decision::Current(&much_newer),
            "nothing is ever downgraded"
        );
        let pre_release = rtk_of("rtk 1.2.0-rc.1");
        assert_eq!(
            decide(Some(&pre_release), target),
            Decision::Current(&pre_release)
        );
        let strange = rtk_of("rtk dev-build");
        assert_eq!(
            decide(Some(&strange), target),
            Decision::Unreadable(&strange)
        );
    }

    #[test]
    fn a_plan_names_the_pinned_asset_and_url() {
        let machine = Fake(vec![(home_var(), "home".into())]);
        let plan = plan(&machine, "linux", "x86_64").unwrap();
        assert_eq!(plan.asset.name, "rtk-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(
            plan.url,
            format!("{RELEASES}/{TAG}/rtk-x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(plan.sha256.len(), 64);
        assert_eq!(plan.source, HashSource::Pinned);
        assert_eq!(plan.edition, Edition::Standard);
        assert_eq!(
            plan.target,
            Path::new("home").join(".local").join("bin").join(rtk::EXE)
        );
        let oss = Fake(vec![
            (home_var(), "home".into()),
            ("RTK_MCP_RTK_EDITION", "oss".into()),
        ]);
        let plan = super::plan(&oss, "windows", "x86_64").unwrap();
        assert_eq!(plan.asset.name, "rtk-oss-x86_64-pc-windows-msvc.zip");
        assert_eq!(plan.edition, Edition::Oss);
        assert!(super::plan(&machine, "plan9", "x86_64")
            .unwrap_err()
            .contains("no prebuilt RTK"));
        assert!(super::plan(&Fake(vec![]), "linux", "x86_64")
            .unwrap_err()
            .contains("RTK_MCP_INSTALL_DIR"));
    }

    #[test]
    fn the_pinned_release_is_always_checked_against_the_compiled_in_hashes() {
        let machine = Fake(vec![(home_var(), "home".into())]);
        let pinned = Release::pinned();
        let expected = checksum_in(CHECKSUMS, &tar_asset().name).unwrap();
        // a checksums.txt that disagrees is not listened to for the release whose hashes we have
        let lie = format!("{}  {}\n", "0".repeat(64), tar_asset().name);
        for published in [None, Some(lie.as_str())] {
            let plan = plan_for(&machine, "linux", "x86_64", &pinned, published, None).unwrap();
            assert_eq!(plan.sha256, expected);
            assert_eq!(plan.source, HashSource::Pinned);
        }
    }

    #[test]
    fn another_release_is_checked_against_its_published_checksums() {
        let machine = Fake(vec![(home_var(), "home".into())]);
        let release = Release::from_tag("v9.9.9").unwrap();
        let name = tar_asset().name;
        let published = format!(
            "{}  {name}\n{}  other.zip\n",
            "c".repeat(64),
            "d".repeat(64)
        );
        let plan = plan_for(
            &machine,
            "linux",
            "x86_64",
            &release,
            Some(published.as_str()),
            None,
        )
        .unwrap();
        assert_eq!(plan.sha256, "c".repeat(64));
        assert_eq!(plan.source, HashSource::Published);
        assert_eq!(plan.url, format!("{RELEASES}/v9.9.9/{name}"));
        // without the file, or without our asset in it, there is nothing to check against
        let err = plan_for(&machine, "linux", "x86_64", &release, None, None).unwrap_err();
        assert!(err.contains("no checksums.txt for v9.9.9"), "{err}");
        let without_ours = format!("{}  other.zip\n", "d".repeat(64));
        let err = plan_for(
            &machine,
            "linux",
            "x86_64",
            &release,
            Some(without_ours.as_str()),
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("lists no rtk-x86_64-unknown-linux-musl"),
            "{err}"
        );
    }

    #[test]
    fn an_upgrade_writes_where_the_old_rtk_is() {
        let machine = Fake(vec![(home_var(), "home".into())]);
        let there = Path::new("somewhere").join("odd-name");
        let plan = plan_for(
            &machine,
            "linux",
            "x86_64",
            &Release::pinned(),
            None,
            Some(&there),
        )
        .unwrap();
        assert_eq!(plan.target, there);
        // and it does not need a home directory for that
        let plan = plan_for(
            &Fake(vec![]),
            "linux",
            "x86_64",
            &Release::pinned(),
            None,
            Some(&there),
        )
        .unwrap();
        assert_eq!(plan.target, there);
    }

    #[test]
    fn the_executable_is_taken_from_a_tar_gz_and_nothing_else() {
        let archive = tar_gz(&[
            ("LICENSE", b"licence text"),
            ("rtk", b"the executable"),
            ("LICENSE-ENGINE", b"more licence text"),
        ]);
        assert_eq!(extract(&tar_asset(), &archive).unwrap(), b"the executable");
    }

    #[test]
    fn a_member_in_a_subdirectory_counts_and_a_lookalike_does_not() {
        let nested = tar_gz(&[("rtk-1.1.0/rtk", b"nested")]);
        assert_eq!(extract(&tar_asset(), &nested).unwrap(), b"nested");
        let lookalike = tar_gz(&[("rtk.sh", b"x"), ("not-rtk", b"y"), ("rtk-notes", b"z")]);
        assert!(extract(&tar_asset(), &lookalike)
            .unwrap_err()
            .contains("holds no rtk"));
    }

    #[test]
    fn a_directory_named_like_the_executable_is_not_the_executable() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "rtk/", std::io::empty())
            .unwrap();
        let archive = gzip(&builder.into_inner().unwrap());
        assert!(extract(&tar_asset(), &archive)
            .unwrap_err()
            .contains("holds no rtk"));
    }

    #[test]
    fn the_executable_is_taken_from_a_zip() {
        let archive = zip_of(&[("LICENSE", b"text"), ("rtk.exe", b"MZ executable")]);
        assert_eq!(extract(&zip_asset(), &archive).unwrap(), b"MZ executable");
        let none = zip_of(&[("readme.txt", b"x")]);
        assert!(extract(&zip_asset(), &none)
            .unwrap_err()
            .contains("holds no rtk.exe"));
    }

    #[test]
    fn archives_that_are_not_archives_are_refused() {
        assert!(extract(&tar_asset(), b"plainly not gzip").is_err());
        assert!(extract(&zip_asset(), b"plainly not a zip").is_err());
        assert!(extract(&tar_asset(), &[]).is_err());
    }

    #[test]
    fn an_empty_member_is_refused() {
        let archive = tar_gz(&[("rtk", b"")]);
        assert!(extract(&tar_asset(), &archive)
            .unwrap_err()
            .contains("empty"));
    }

    /// A plan whose expected hash is the hash of `archive`, writing to `target`.
    fn plan_for_archive(archive: &[u8], target: &Path) -> Plan {
        Plan {
            edition: Edition::Standard,
            asset: tar_asset(),
            release: Release::from_tag("v9.9.9").unwrap(),
            url: String::new(),
            sha256: sha256_hex(archive),
            source: HashSource::Published,
            target: target.to_path_buf(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtk-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A probe that finds a working RTK of the given version in whatever file it is shown.
    fn works(version: &'static str) -> impl Fn(&Path) -> Option<String> {
        move |_| Some(version.to_string())
    }

    fn names_in(dir: &Path) -> Vec<OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_matching_archive_is_installed_atomically_and_executable() {
        let archive = tar_gz(&[("LICENSE", b"l"), ("rtk", b"#!/bin/sh\necho hi\n")]);
        let dir = scratch("install-ok").join("nested").join("bin");
        let target = dir.join(rtk::EXE);
        let version = finish(
            &plan_for_archive(&archive, &target),
            &archive,
            None,
            &works("rtk 2.0.0"),
        )
        .unwrap();
        assert_eq!(version, "rtk 2.0.0");
        assert_eq!(std::fs::read(&target).unwrap(), b"#!/bin/sh\necho hi\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // the only thing in the directory is the executable: no temporary file is left behind
        assert_eq!(names_in(&dir), [OsString::from(rtk::EXE)]);
        // installing over an existing file replaces it
        let again = tar_gz(&[("rtk", b"second")]);
        finish(
            &plan_for_archive(&again, &target),
            &again,
            None,
            &works("rtk 2.0.1"),
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert_eq!(names_in(&dir), [OsString::from(rtk::EXE)]);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn the_new_executable_is_run_before_it_replaces_the_old_one() {
        let dir = scratch("upgrade-order");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(rtk::EXE);
        std::fs::write(&target, b"old rtk").unwrap();
        let archive = tar_gz(&[("rtk", b"new rtk")]);
        let seen = std::cell::RefCell::new(None);
        let probe = |staged: &Path| {
            // at this moment the new file is next to the old one, which has not been touched
            *seen.borrow_mut() = Some((
                staged.to_path_buf(),
                std::fs::read(staged).unwrap(),
                std::fs::read(&target).unwrap(),
            ));
            Some("rtk 1.1.0".to_string())
        };
        let version = finish(
            &plan_for_archive(&archive, &target),
            &archive,
            Some(Version(0, 49, 0)),
            &probe,
        )
        .unwrap();
        assert_eq!(version, "rtk 1.1.0");
        let (staged, new, old) = seen.into_inner().expect("the probe ran");
        assert_eq!(new, b"new rtk");
        assert_eq!(old, b"old rtk");
        assert_eq!(staged.parent(), Some(dir.as_path()), "same directory");
        assert_ne!(staged, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"new rtk");
        assert_eq!(
            names_in(&dir),
            [OsString::from(rtk::EXE)],
            "nothing is left over"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_rtk_that_does_not_run_never_replaces_one_that_does() {
        let dir = scratch("upgrade-broken");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(rtk::EXE);
        std::fs::write(&target, b"old rtk").unwrap();
        let archive = tar_gz(&[("rtk", b"broken rtk")]);
        let err = finish(
            &plan_for_archive(&archive, &target),
            &archive,
            Some(Version(0, 49, 0)),
            &|_: &Path| None,
        )
        .unwrap_err();
        assert!(err.contains("does not run on this machine"), "{err}");
        assert!(err.contains("was left as it was"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old rtk");
        assert_eq!(names_in(&dir), [OsString::from(rtk::EXE)], "no staged file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_reports_an_older_version_does_not_replace_the_one_in_use() {
        let dir = scratch("upgrade-older");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(rtk::EXE);
        std::fs::write(&target, b"old rtk").unwrap();
        let archive = tar_gz(&[("rtk", b"strange rtk")]);
        let err = finish(
            &plan_for_archive(&archive, &target),
            &archive,
            Some(Version(1, 1, 0)),
            &works("rtk 1.0.0"),
        )
        .unwrap_err();
        assert!(err.contains("older than the 1.1.0 in use"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old rtk");
        assert_eq!(names_in(&dir), [OsString::from(rtk::EXE)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_mismatching_archive_writes_nothing() {
        let archive = tar_gz(&[("rtk", b"real")]);
        let evil = tar_gz(&[("rtk", b"evil")]);
        let dir = scratch("install-bad").join("bin");
        let target = dir.join(rtk::EXE);
        let err = finish(
            &plan_for_archive(&archive, &target),
            &evil,
            None,
            &works("rtk 2.0.0"),
        )
        .unwrap_err();
        assert!(err.contains("does not match the checksum"), "{err}");
        assert!(err.contains("published with v9.9.9"), "{err}");
        assert!(err.contains("nothing was installed"), "{err}");
        assert!(!dir.exists(), "not even the directory is created");
        // for the pinned release the message names rtk-mcp as the source of the hash
        let mut pinned = plan_for_archive(&archive, &target);
        pinned.source = HashSource::Pinned;
        let err = finish(&pinned, &evil, None, &works("rtk 2.0.0")).unwrap_err();
        assert!(
            err.contains("does not match the checksum pinned in rtk-mcp"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_mismatching_archive_leaves_an_installed_rtk_alone() {
        let dir = scratch("upgrade-bad-hash");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(rtk::EXE);
        std::fs::write(&target, b"old rtk").unwrap();
        let archive = tar_gz(&[("rtk", b"real")]);
        let evil = tar_gz(&[("rtk", b"evil")]);
        finish(
            &plan_for_archive(&archive, &target),
            &evil,
            Some(Version(0, 49, 0)),
            &works("rtk 2.0.0"),
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&target).unwrap(), b"old rtk");
        assert_eq!(names_in(&dir), [OsString::from(rtk::EXE)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let archive = tar_gz(&[("rtk", b"x")]);
        let dir = scratch("install-blocked");
        std::fs::create_dir_all(&dir).unwrap();
        // a directory named like the executable makes the final rename fail
        std::fs::create_dir_all(dir.join(rtk::EXE)).unwrap();
        let target = dir.join(rtk::EXE);
        let err = finish(
            &plan_for_archive(&archive, &target),
            &archive,
            None,
            &works("rtk 2.0.0"),
        )
        .unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
        let leftovers: Vec<_> = names_in(&dir)
            .into_iter()
            .filter(|name| name != rtk::EXE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plain_file_in_a_writable_directory_can_be_replaced() {
        let dir = scratch("replaceable-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rtk");
        std::fs::write(&file, b"x").unwrap();
        replaceable(&file).unwrap();
        assert_eq!(
            names_in(&dir),
            [OsString::from("rtk")],
            "the check leaves nothing"
        );
        // not a file at all
        let err = replaceable(&dir).unwrap_err();
        assert!(err.contains("not a regular file"), "{err}");
        let err = replaceable(&dir.join("missing")).unwrap_err();
        assert!(err.contains("cannot be looked at"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_replaced() {
        let dir = scratch("replaceable-link");
        let cellar = dir.join("Cellar").join("rtk").join("1.0.0").join("bin");
        std::fs::create_dir_all(&cellar).unwrap();
        let real = cellar.join("rtk");
        std::fs::write(&real, b"homebrew's").unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let link = bin.join("rtk");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = replaceable(&link).unwrap_err();
        assert!(err.contains("is a symbolic link to"), "{err}");
        assert!(err.contains("package manager"), "{err}");
        assert!(err.contains("brew upgrade rtk"), "{err}");
        // and the file behind it is the real one, untouched by the check
        assert_eq!(std::fs::read(&real).unwrap(), b"homebrew's");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scoop_shim_is_not_replaced() {
        let dir = scratch("replaceable-shim");
        std::fs::create_dir_all(&dir).unwrap();
        let launcher = dir.join("rtk.exe");
        std::fs::write(&launcher, b"launcher").unwrap();
        // without the shim file next to it, it is an ordinary file
        replaceable(&launcher).unwrap();
        std::fs::write(
            dir.join("rtk.shim"),
            b"path = C:\\scoop\\apps\\rtk\\current\\rtk.exe\n",
        )
        .unwrap();
        let err = replaceable(&launcher).unwrap_err();
        assert!(err.contains("is a Scoop shim"), "{err}");
        assert!(err.contains("rtk.shim"), "{err}");
        assert!(err.contains("scoop update rtk"), "{err}");
        assert_eq!(std::fs::read(&launcher).unwrap(), b"launcher");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_package_manager_hint_follows_the_path() {
        let hint =
            |link: &str, behind: &str| manager_hint(Path::new(link), Some(Path::new(behind)));
        assert!(hint(
            "/opt/homebrew/bin/rtk",
            "/opt/homebrew/Cellar/rtk/1.0/bin/rtk"
        )
        .contains("brew"));
        assert!(hint("/usr/local/bin/rtk", "/usr/local/Cellar/rtk/1.0/bin/rtk").contains("brew"));
        assert!(hint("/home/u/.linuxbrew/bin/rtk", "/x").contains("brew"));
        assert!(hint(
            r"C:\Users\u\AppData\Local\Microsoft\WinGet\Links\rtk.exe",
            r"C:\Users\u\AppData\Local\Microsoft\WinGet\Packages\rtk\rtk.exe"
        )
        .contains("winget upgrade rtk-ai.rtk"));
        assert!(hint(r"C:\Users\u\scoop\shims\rtk.exe", r"C:\x").contains("scoop update"));
        assert!(hint("/usr/bin/rtk", "/opt/thing/rtk").contains("the way you installed it"));
        assert!(manager_hint(Path::new("/usr/bin/rtk"), None).contains("the way you installed it"));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_written_to_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("replaceable-readonly");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rtk");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // root writes anywhere, so there is nothing to see when the tests run as root
        let writable_anyway = std::fs::write(dir.join(".probe"), b"").is_ok();
        if writable_anyway {
            let _ = std::fs::remove_file(dir.join(".probe"));
        } else {
            let err = replaceable(&file).unwrap_err();
            assert!(err.contains("cannot write in"), "{err}");
            assert!(err.contains("the way you installed it"), "{err}");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn done(source: HashSource, label: &str) -> Done {
        let mut plan = plan_for_archive(b"x", Path::new("/p/rtk"));
        plan.release = Release::from_tag("v1.1.0").unwrap();
        plan.source = source;
        plan.sha256 = "ab".repeat(32);
        Done {
            rtk: Rtk {
                path: PathBuf::from("/p/rtk"),
                version: "rtk 1.1.0".to_string(),
                origin: Origin::Installed,
            },
            plan,
            label: label.to_string(),
        }
    }

    fn target(label: &str) -> Target {
        Target {
            release: Release::from_tag("v1.1.0").unwrap(),
            published: None,
            label: label.to_string(),
        }
    }

    #[test]
    fn the_messages_say_what_happened() {
        let installed = Outcome::Installed(done(HashSource::Pinned, "the latest release"))
            .report()
            .unwrap();
        assert!(
            installed.starts_with("Installed rtk 1.1.0 at /p/rtk ("),
            "{installed}"
        );
        assert!(
            installed.contains("v1.1.0, the latest release"),
            "{installed}"
        );
        assert!(
            installed.contains("the one pinned in rtk-mcp"),
            "{installed}"
        );
        assert!(installed.contains("no restart needed"), "{installed}");
        assert!(installed.contains("Edition: standard"), "{installed}");

        let upgraded = Outcome::Upgraded {
            from: "rtk 0.49.0".to_string(),
            done: done(HashSource::Published, "the latest release"),
        }
        .report()
        .unwrap();
        assert!(
            upgraded.starts_with("Upgraded RTK from 0.49.0 to 1.1.0 at /p/rtk ("),
            "{upgraded}"
        );
        assert!(
            upgraded.contains("the checksums.txt published with v1.1.0"),
            "{upgraded}"
        );
        assert!(
            upgraded.contains("only after the new one had run successfully"),
            "{upgraded}"
        );

        let current = Outcome::Current {
            rtk: rtk_of("rtk 1.1.0"),
            target: target("the latest release"),
        }
        .report()
        .unwrap();
        assert!(
            current.contains("already installed and up to date: rtk 1.1.0 at /x/rtk"),
            "{current}"
        );
        assert!(
            current.contains("not older than v1.1.0, the latest release"),
            "{current}"
        );
        assert!(current.ends_with("Nothing to install."), "{current}");

        let left = Outcome::Left {
            rtk: rtk_of("rtk 0.49.0"),
            target: target("the latest release"),
            why: "it is a symbolic link".to_string(),
        }
        .report()
        .unwrap_err();
        assert!(
            left.starts_with("RTK was not upgraded. rtk 0.49.0 at /x/rtk is older than v1.1.0"),
            "{left}"
        );
        assert!(left.ends_with("it is a symbolic link."), "{left}");
    }

    #[test]
    fn the_log_lines_start_with_what_happened() {
        let installed = Outcome::Installed(done(HashSource::Pinned, "x")).log_line();
        assert!(
            installed.starts_with("installed rtk 1.1.0 at /p/rtk"),
            "{installed}"
        );
        let upgraded = Outcome::Upgraded {
            from: "rtk 0.49.0".to_string(),
            done: done(HashSource::Pinned, "x"),
        }
        .log_line();
        assert!(
            upgraded.starts_with("upgraded rtk 0.49.0 to rtk 1.1.0 at /p/rtk"),
            "{upgraded}"
        );
        let current = Outcome::Current {
            rtk: rtk_of("rtk 1.1.0"),
            target: target("the latest release"),
        }
        .log_line();
        assert!(current.contains("is up to date"), "{current}");
        let left = Outcome::Left {
            rtk: rtk_of("rtk 0.49.0"),
            target: target("the latest release"),
            why: "reason".to_string(),
        }
        .log_line();
        assert!(left.contains("was left alone: reason"), "{left}");
    }

    #[test]
    fn only_one_install_runs_at_a_time_and_a_dropped_one_lets_go() {
        let first = Running::take().expect("nothing runs yet");
        assert!(Running::take().is_none(), "a second one is turned away");
        drop(first);
        let again = Running::take();
        assert!(again.is_some(), "dropping the first frees the slot");
    }
}
