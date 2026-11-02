//! Installing RTK from its GitHub release, for the `install_rtk` tool and `RTK_MCP_AUTO_INSTALL`.
//!
//! No installer script runs and no shell is involved. The release asset for this platform is
//! fetched over HTTPS with `curl` (on macOS, on Windows 10 and later, and on nearly every Linux),
//! compared with a SHA-256 that is compiled into this program, unpacked in memory, and only the
//! `rtk` executable is written, atomically, into one directory. A download whose hash is not the
//! pinned one is discarded unread, whoever served it: a compromised mirror, a swapped release asset
//! or a man in the middle gets nothing past the comparison.
//!
//! Which release is installed is a property of this build of rtk-mcp, not of the day it runs:
//! [`TAG`], and the hashes in `data/rtk-checksums-<tag>.txt`. A newer RTK arrives with a newer
//! rtk-mcp.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::rtk::{self, Origin, Rtk};
use crate::shell::{Machine, RealMachine};

/// One place for the release tag: it names the checksums file as well as the download.
macro_rules! tag {
    () => {
        "v1.1.0"
    };
}

/// The RTK release this build installs.
pub const TAG: &str = tag!();

/// The `checksums.txt` that release was published with, byte for byte. CI compares it with the
/// one on GitHub, so a re-uploaded asset is noticed.
const CHECKSUMS: &str = include_str!(concat!("../data/rtk-checksums-", tag!(), ".txt"));

const RELEASES: &str = "https://github.com/rtk-ai/rtk/releases/download";

/// The largest executable that is unpacked (RTK is about 15 MB), and the largest archive taken.
const MAX_EXE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_ARCHIVE_BYTES: usize = 100 * 1024 * 1024;

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

/// Where `install_rtk` puts the executable: `RTK_MCP_INSTALL_DIR`, else `~/.local/bin`
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

/// Everything settled before anything is downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub asset: Asset,
    pub url: String,
    /// The pinned SHA-256 of the asset.
    pub sha256: String,
    pub dir: PathBuf,
}

pub fn plan(machine: &dyn Machine, os: &str, arch: &str) -> Result<Plan, String> {
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
    let sha256 = checksum_in(CHECKSUMS, &asset.name)
        .ok_or_else(|| format!("rtk-mcp has no pinned checksum for {}", asset.name))?;
    let dir = install_dir(machine).ok_or_else(|| {
        "cannot tell where to install RTK: HOME is not set; set RTK_MCP_INSTALL_DIR".to_string()
    })?;
    let url = format!("{RELEASES}/{TAG}/{}", asset.name);
    Ok(Plan {
        asset,
        url,
        sha256,
        dir,
    })
}

/// Check a downloaded archive against the pinned hash, unpack the executable and write it into
/// the install directory. Returns the path written. Nothing is written unless the hash matches.
pub fn finish(plan: &Plan, archive: &[u8]) -> Result<PathBuf, String> {
    let actual = sha256_hex(archive);
    if actual != plan.sha256 {
        return Err(format!(
            "the download of {} does not match the checksum pinned in rtk-mcp (expected {}, got \
             {actual}); nothing was installed",
            plan.asset.name, plan.sha256
        ));
    }
    let exe = extract(&plan.asset, archive)?;
    write_executable(&plan.dir, &exe)
}

/// Write `bytes` as the RTK executable in `dir`: to a temporary file next to it first, then
/// renamed into place, so a reader never sees half a file.
fn write_executable(dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let target = dir.join(rtk::EXE);
    let temp = dir.join(format!(".{}.{}.part", rtk::EXE, std::process::id()));
    let write = || -> std::io::Result<()> {
        std::fs::write(&temp, bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&temp, &target)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot write {}: {e}", target.display())
    })?;
    Ok(target)
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// `RTK_MCP_NO_INSTALL=1`: this server never downloads anything, and has no `install_rtk` tool.
pub fn disabled() -> bool {
    flag("RTK_MCP_NO_INSTALL")
}

/// `RTK_MCP_AUTO_INSTALL=1`: install RTK in the background at startup when none is found.
pub fn auto_enabled() -> bool {
    flag("RTK_MCP_AUTO_INSTALL") && !disabled()
}

/// Fetch `url` with curl. TLS 1.2 or later, https only (also across redirects, which GitHub uses
/// to hand the asset to its storage), and limits on size and time. The proxy settings of the
/// environment apply as they do for any curl.
async fn download(url: &str) -> Result<Vec<u8>, String> {
    let program = if cfg!(windows) { "curl.exe" } else { "curl" };
    let mut command = Command::new(program);
    command
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "20",
            "--max-time",
            "150",
            "--max-filesize",
            "104857600",
            "--user-agent",
            concat!("rtk-mcp/", env!("CARGO_PKG_VERSION")),
        ])
        .arg(url)
        .stdin(Stdio::null()) // never the MCP transport
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(crate::shell::CREATE_NO_WINDOW);
    let output = command.output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            format!(
                "cannot download: `{program}` is not installed. Install RTK by hand instead \
                 ({url}), or install curl"
            )
        } else {
            format!("cannot run {program}: {e}")
        }
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        return Err(format!("downloading {url} failed: {}", reason.trim()));
    }
    if output.stdout.len() > MAX_ARCHIVE_BYTES {
        return Err(format!("{url} is larger than {MAX_ARCHIVE_BYTES} bytes"));
    }
    Ok(output.stdout)
}

/// What an install did.
#[derive(Debug, Clone)]
pub struct Done {
    pub rtk: Rtk,
    pub asset: String,
    pub sha256: String,
}

/// Set while an install runs, so a second call (the model's, and the automatic one at startup)
/// does not race the first over the same file.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Download, verify and install RTK, and make it the RTK in use.
pub async fn run() -> Result<Done, String> {
    if disabled() {
        return Err("installing RTK is turned off here (RTK_MCP_NO_INSTALL is set)".to_string());
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("another install of RTK is already running; try again in a minute".to_string());
    }
    let result = install().await;
    RUNNING.store(false, Ordering::SeqCst);
    result
}

async fn install() -> Result<Done, String> {
    let plan = plan(&RealMachine, std::env::consts::OS, std::env::consts::ARCH)?;
    tracing::info!("downloading {}", plan.url);
    let archive = download(&plan.url).await?;
    let for_blocking = plan.clone();
    let (path, version) = tokio::task::spawn_blocking(move || {
        let path = finish(&for_blocking, &archive)?;
        match rtk::probe(&path) {
            Some(version) => Ok((path, version)),
            None => {
                let _ = std::fs::remove_file(&path);
                Err(format!(
                    "{} was installed but does not run on this machine, so it was removed; \
                     install RTK by hand (https://github.com/rtk-ai/rtk)",
                    path.display()
                ))
            }
        }
    })
    .await
    .map_err(|e| format!("the install task failed: {e}"))??;
    let rtk = Rtk {
        path,
        version,
        origin: Origin::Installed,
    };
    rtk::set_current(Some(rtk.clone()));
    Ok(Done {
        rtk,
        asset: plan.asset.name,
        sha256: plan.sha256,
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
    fn a_plan_names_the_pinned_asset_and_url() {
        let machine = Fake(vec![(home_var(), "home".into())]);
        let plan = plan(&machine, "linux", "x86_64").unwrap();
        assert_eq!(plan.asset.name, "rtk-x86_64-unknown-linux-musl.tar.gz");
        assert_eq!(
            plan.url,
            format!("{RELEASES}/{TAG}/rtk-x86_64-unknown-linux-musl.tar.gz")
        );
        assert_eq!(plan.sha256.len(), 64);
        let oss = Fake(vec![
            (home_var(), "home".into()),
            ("RTK_MCP_RTK_EDITION", "oss".into()),
        ]);
        let plan = super::plan(&oss, "windows", "x86_64").unwrap();
        assert_eq!(plan.asset.name, "rtk-oss-x86_64-pc-windows-msvc.zip");
        assert!(super::plan(&machine, "plan9", "x86_64")
            .unwrap_err()
            .contains("no prebuilt RTK"));
        assert!(super::plan(&Fake(vec![]), "linux", "x86_64")
            .unwrap_err()
            .contains("RTK_MCP_INSTALL_DIR"));
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

    /// A plan whose pinned hash is the hash of `archive`, writing into `dir`.
    fn plan_for(archive: &[u8], dir: &Path) -> Plan {
        Plan {
            asset: tar_asset(),
            url: String::new(),
            sha256: sha256_hex(archive),
            dir: dir.to_path_buf(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rtk-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_matching_archive_is_installed_atomically_and_executable() {
        let archive = tar_gz(&[("LICENSE", b"l"), ("rtk", b"#!/bin/sh\necho hi\n")]);
        let dir = scratch("install-ok").join("nested").join("bin");
        let path = finish(&plan_for(&archive, &dir), &archive).unwrap();
        assert_eq!(path, dir.join(rtk::EXE));
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\necho hi\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        // the only thing in the directory is the executable: no temporary file is left behind
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [OsString::from(rtk::EXE)]);
        // installing over an existing file replaces it
        let again = tar_gz(&[("rtk", b"second")]);
        let path = finish(&plan_for(&again, &dir), &again).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn a_mismatching_archive_writes_nothing() {
        let archive = tar_gz(&[("rtk", b"real")]);
        let evil = tar_gz(&[("rtk", b"evil")]);
        let dir = scratch("install-bad").join("bin");
        let err = finish(&plan_for(&archive, &dir), &evil).unwrap_err();
        assert!(err.contains("does not match the checksum pinned"), "{err}");
        assert!(err.contains("nothing was installed"), "{err}");
        assert!(!dir.exists(), "not even the directory is created");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let archive = tar_gz(&[("rtk", b"x")]);
        let dir = scratch("install-blocked");
        std::fs::create_dir_all(&dir).unwrap();
        // a directory named like the executable makes the final rename fail
        std::fs::create_dir_all(dir.join(rtk::EXE)).unwrap();
        let err = finish(&plan_for(&archive, &dir), &archive).unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != rtk::EXE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
