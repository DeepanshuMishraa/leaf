//! `leaf upgrade`: replaces this binary with the newest GitHub release.
//!
//! The release workflow (`.github/workflows/release.yml`) publishes one
//! `leaf-<target>.tar.gz` per platform, each beside a `.sha256` file. Asset
//! names carry no version, and the newest version is read from where
//! `releases/latest` redirects, so no GitHub API call (or its rate limit) is
//! involved. The download is checked against its checksum and run once before
//! it replaces the binary, and the swap is a rename, so a failure at any step
//! leaves the installed leaf as it was.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::spinner;

const REPO: &str = "DeepanshuMishraa/leaf";
const USER_AGENT: &str = concat!("leaf/", env!("CARGO_PKG_VERSION"));
/// A release archive larger than this is refused rather than held in memory.
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;

/// The release target this binary was built for, as the workflow names its archive.
pub const TARGET: Option<&str> = if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "x86_64",
    target_env = "gnu"
)) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(
    target_os = "linux",
    target_arch = "aarch64",
    target_env = "gnu"
)) {
    Some("aarch64-unknown-linux-gnu")
} else {
    None
};

/// A release version: `major.minor.patch`, compared in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    /// Reads `1.2.3` or `v1.2.3`; anything else (a pre-release suffix too) is `None`.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let mut parts = text.split('.').map(|part| {
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse::<u64>().ok())
                .flatten()
        });
        let version = Version {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
        };
        parts.next().is_none().then_some(version)
    }

    /// The version this binary was built as; `None` for a pre-release
    /// (`1.2.0-rc.1`), which `leaf upgrade` leaves alone.
    pub fn current() -> Option<Version> {
        Version::parse(env!("CARGO_PKG_VERSION"))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpgradeFailed {
    #[error(
        "Prebuilt leaf binaries cover macOS and Linux (glibc) on x86_64 and arm64, not this machine. Build it from source instead: cargo install leaf-reader"
    )]
    UnsupportedPlatform,
    #[error(
        "This leaf ({0}) is a pre-release build, and leaf upgrade only moves between releases. Install the newest release with: curl -fsSL https://leaf.dipxsy.app | sh"
    )]
    PreRelease(&'static str),
    #[error(
        "Couldn't reach GitHub to find the newest release ({0}). Nothing was changed; check the connection and run leaf upgrade again."
    )]
    Offline(String),
    #[error(
        "GitHub's newest release ({0}) doesn't look like a leaf version. Nothing was changed; see https://github.com/{REPO}/releases"
    )]
    UnreadableRelease(String),
    #[error(
        "No leaf release has been published on GitHub yet, so there's nothing to upgrade to. This leaf is unchanged."
    )]
    NoRelease,
    #[error(
        "Couldn't download {url} ({cause}). Nothing was changed; run leaf upgrade again, or download it from https://github.com/{REPO}/releases"
    )]
    Download { url: String, cause: String },
    #[error(
        "The downloaded archive doesn't match its published checksum (expected {expected}, got {actual}). It was thrown away and nothing was changed; run leaf upgrade again."
    )]
    Checksum { expected: String, actual: String },
    #[error(
        "Couldn't unpack the downloaded archive ({0}). Nothing was changed; run leaf upgrade again."
    )]
    Unpack(String),
    #[error(
        "The downloaded leaf {version} didn't run here ({cause}), so it wasn't installed. The current leaf is unchanged."
    )]
    Broken { version: Version, cause: String },
    #[error(
        "This leaf was installed with Homebrew, at {0}, so Homebrew upgrades it. Nothing was changed; run: brew upgrade leaf"
    )]
    Homebrew(PathBuf),
    #[error("Couldn't find where this leaf is installed ({0}). Nothing was changed.")]
    NoSelf(String),
    #[error(
        "Can't replace {path} ({cause}). Nothing was changed. If a package manager (Nix, Homebrew) installed leaf, upgrade it there; if the folder needs root, run: sudo leaf upgrade"
    )]
    Replace { path: PathBuf, cause: String },
}

/// What `leaf upgrade` found or did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Already the newest, or a local build ahead of it.
    Current {
        installed: Version,
        newest: Version,
    },
    /// `--check` only: a newer release is out.
    Available {
        from: Version,
        to: Version,
    },
    Upgraded {
        from: Version,
        to: Version,
    },
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Current { installed, newest } if installed > newest => write!(
                f,
                "This leaf ({installed}) is newer than the newest release ({newest}); nothing to do."
            ),
            Outcome::Current { newest, .. } => {
                write!(
                    f,
                    "leaf {newest} is the newest release, and it's installed."
                )
            }
            Outcome::Available { from, to } => {
                write!(
                    f,
                    "leaf {to} is out (this is {from}). Run leaf upgrade to install it."
                )
            }
            Outcome::Upgraded { from, to } => write!(f, "Upgraded leaf from {from} to {to}."),
        }
    }
}

/// Finds the newest release and, unless `check_only`, installs it over this binary.
pub fn run(check_only: bool) -> Result<Outcome, UpgradeFailed> {
    let target = TARGET.ok_or(UpgradeFailed::UnsupportedPlatform)?;
    let current = Version::current().ok_or(UpgradeFailed::PreRelease(env!("CARGO_PKG_VERSION")))?;
    let newest = spinner::step("Checking for a new release", |_| latest())?;
    if newest <= current {
        return Ok(Outcome::Current {
            installed: current,
            newest,
        });
    }
    if check_only {
        return Ok(Outcome::Available {
            from: current,
            to: newest,
        });
    }
    // Found first, so an unwritable install fails before anything is downloaded.
    let exe = installed_path()?;
    let base =
        format!("https://github.com/{REPO}/releases/download/v{newest}/leaf-{target}.tar.gz");
    let archive = spinner::step(&format!("Downloading leaf {newest}"), |spinner| {
        download(&base, |got, total| {
            spinner.detail(match total {
                Some(total) => format!(
                    "({} of {})",
                    spinner::megabytes(got),
                    spinner::megabytes(total)
                ),
                None => format!("({})", spinner::megabytes(got)),
            })
        })
    })?;
    spinner::step("Verifying the download", |_| {
        let sums = download(&format!("{base}.sha256"), |_, _| {})?;
        let expected = checksum_of(&String::from_utf8_lossy(&sums)).ok_or_else(|| {
            UpgradeFailed::Download {
                url: format!("{base}.sha256"),
                cause: "it holds no SHA-256 checksum".to_owned(),
            }
        })?;
        let actual = hex(&Sha256::digest(&archive));
        if actual != expected {
            return Err(UpgradeFailed::Checksum { expected, actual });
        }
        Ok(())
    })?;
    spinner::step(&format!("Installing to {}", exe.display()), |_| {
        install(&archive, &exe, newest)
    })?;
    Ok(Outcome::Upgraded {
        from: current,
        to: newest,
    })
}

/// The newest release's version, from where `releases/latest` redirects
/// (`…/releases/tag/v1.2.3`). Drafts and pre-releases are never "latest".
fn latest() -> Result<Version, UpgradeFailed> {
    let response = ureq::get(&format!("https://github.com/{REPO}/releases/latest"))
        .config()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .header("user-agent", USER_AGENT)
        .call()
        .map_err(|e| UpgradeFailed::Offline(e.to_string()))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| {
            UpgradeFailed::Offline(format!(
                "GitHub answered {} without pointing to a release; there may be none yet",
                response.status()
            ))
        })?;
    // With no release yet, GitHub sends `latest` to the releases list instead.
    if location.trim_end_matches('/').ends_with("/releases") {
        return Err(UpgradeFailed::NoRelease);
    }
    tag_version(location).ok_or_else(|| UpgradeFailed::UnreadableRelease(location.to_owned()))
}

/// The version in a release page's link: `…/releases/tag/v1.2.3`.
fn tag_version(location: &str) -> Option<Version> {
    let (_, tag) = location.rsplit_once("/releases/tag/")?;
    Version::parse(tag.split(['?', '#']).next()?)
}

/// Downloads `url`, telling `progress` the bytes so far and the size, when
/// the server says it.
fn download(url: &str, progress: impl Fn(u64, Option<u64>)) -> Result<Vec<u8>, UpgradeFailed> {
    let failed = |cause: String| UpgradeFailed::Download {
        url: url.to_owned(),
        cause,
    };
    let body = ureq::get(url)
        .header("user-agent", USER_AGENT)
        .call()
        .map_err(|e| failed(e.to_string()))?
        .into_body();
    let total = body.content_length();
    let mut reader = body.into_with_config().limit(MAX_ARCHIVE).reader();
    let mut bytes = Vec::with_capacity(total.unwrap_or(0).min(MAX_ARCHIVE) as usize);
    let mut chunk = [0; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk).map_err(|e| failed(e.to_string()))?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&chunk[..read]);
        progress(bytes.len() as u64, total);
    }
}

/// The checksum in a `sha256sum`-style line: `<64 hex digits>  <file>`.
fn checksum_of(text: &str) -> Option<String> {
    let digest = text.split_whitespace().next()?.to_ascii_lowercase();
    (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())).then_some(digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The binary to replace: this one, with symlinks followed, so a link in a
/// `bin` folder keeps pointing at the upgraded file.
fn installed_path() -> Result<PathBuf, UpgradeFailed> {
    let exe = std::env::current_exe().map_err(|e| UpgradeFailed::NoSelf(e.to_string()))?;
    let exe = exe
        .canonicalize()
        .map_err(|e| UpgradeFailed::NoSelf(format!("{}: {e}", exe.display())))?;
    if is_homebrew(&exe) {
        return Err(UpgradeFailed::Homebrew(exe));
    }
    let dir = exe
        .parent()
        .ok_or_else(|| UpgradeFailed::NoSelf(format!("{} has no folder", exe.display())))?;
    // Writing the folder is what the swap needs; probe it before downloading.
    let probe = dir.join(format!(".leaf-upgrade-probe-{}", std::process::id()));
    std::fs::write(&probe, b"")
        .and_then(|()| std::fs::remove_file(&probe))
        .map_err(|e| UpgradeFailed::Replace {
            path: exe.clone(),
            cause: e.to_string(),
        })?;
    Ok(exe)
}

/// Whether `exe` is a Homebrew keg's, which only `brew upgrade` should replace.
fn is_homebrew(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|part| part.as_os_str()).collect();
    parts
        .windows(2)
        .any(|pair| pair[0] == "Cellar" && pair[1] == "leaf")
}

/// Unpacks `archive`, runs the leaf in it, and renames it over `exe`.
fn install(archive: &[u8], exe: &Path, version: Version) -> Result<(), UpgradeFailed> {
    let work = std::env::temp_dir().join(format!("leaf-upgrade-{}", std::process::id()));
    let result = unpack_and_swap(archive, exe, version, &work);
    let _ = std::fs::remove_dir_all(&work);
    result
}

fn unpack_and_swap(
    archive: &[u8],
    exe: &Path,
    version: Version,
    work: &Path,
) -> Result<(), UpgradeFailed> {
    let unpack = |e: std::io::Error| UpgradeFailed::Unpack(e.to_string());
    let _ = std::fs::remove_dir_all(work);
    std::fs::create_dir_all(work).map_err(unpack)?;
    let tarball = work.join("leaf.tar.gz");
    std::fs::write(&tarball, archive).map_err(unpack)?;
    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(work)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| UpgradeFailed::Unpack(format!("tar didn't run: {e}")))?;
    if !output.status.success() {
        return Err(UpgradeFailed::Unpack(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let unpacked = work.join("leaf");
    if !unpacked.is_file() {
        return Err(UpgradeFailed::Unpack(
            "the archive has no leaf binary".to_owned(),
        ));
    }

    // Staged next to the binary, so the swap is a rename within one folder.
    let dir = exe.parent().unwrap_or(Path::new("."));
    let staged = dir.join(format!(".leaf-upgrade-{}", std::process::id()));
    let replace = |cause: String| UpgradeFailed::Replace {
        path: exe.to_owned(),
        cause,
    };
    std::fs::copy(&unpacked, &staged).map_err(|e| replace(e.to_string()))?;
    let swapped = prepare(&staged, version)
        .and_then(|()| std::fs::rename(&staged, exe).map_err(|e| replace(e.to_string())));
    if swapped.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    swapped
}

/// Makes the staged binary executable and runs it, so one that can't run
/// here (wrong architecture, too old a glibc) is never installed.
fn prepare(staged: &Path, version: Version) -> Result<(), UpgradeFailed> {
    use std::os::unix::fs::PermissionsExt;

    let broken = |cause: String| UpgradeFailed::Broken { version, cause };
    std::fs::set_permissions(staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| broken(e.to_string()))?;
    let output = Command::new(staged)
        .arg("version")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| broken(e.to_string()))?;
    let said = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || Version::parse(said.trim().trim_start_matches("leaf ")) != Some(version)
    {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().next().unwrap_or(said.trim());
        return Err(broken(format!("it said {:?}", detail.trim())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_and_order() {
        let v = |s| Version::parse(s);
        assert_eq!(
            v("v0.12.0").map(|x| x.to_string()),
            Some("0.12.0".to_owned())
        );
        assert_eq!(
            v("1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("0.99.99"));
        for bad in ["", "1.2", "1.2.3.4", "1.2.x", "1.2.3-rc.1", "v", "1..2"] {
            assert_eq!(v(bad), None, "{bad:?}");
        }
        assert_eq!(
            Version::current().map(|v| v.to_string()).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn the_newest_version_comes_from_the_release_link() {
        assert_eq!(
            tag_version("https://github.com/o/r/releases/tag/v0.12.0"),
            Version::parse("0.12.0")
        );
        assert_eq!(tag_version("https://github.com/o/r/releases"), None);
        assert_eq!(
            tag_version("https://github.com/o/r/releases/tag/nightly"),
            None
        );
    }

    #[test]
    fn checksum_files_are_read_like_sha256sum_writes_them() {
        let digest = "a".repeat(64);
        assert_eq!(
            checksum_of(&format!("{digest}  leaf.tar.gz\n")),
            Some(digest.clone())
        );
        assert_eq!(checksum_of(&digest.to_uppercase()), Some(digest));
        assert_eq!(checksum_of("abc  leaf.tar.gz"), None);
        assert_eq!(checksum_of(""), None);
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_leaf_in_homebrews_cellar_is_left_to_brew() {
        let brewed = |path: &str| is_homebrew(Path::new(path));
        assert!(brewed("/opt/homebrew/Cellar/leaf/0.12.1/bin/leaf"));
        assert!(brewed(
            "/home/linuxbrew/.linuxbrew/Cellar/leaf/0.12.1/bin/leaf"
        ));
        assert!(!brewed("/Users/me/.local/bin/leaf"));
        assert!(!brewed("/Users/me/.cargo/bin/leaf"));
    }

    #[test]
    fn an_archive_without_leaf_or_that_cant_run_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("leaf-upgrade-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let exe = dir.join("leaf");
        std::fs::write(&exe, b"old").expect("old binary");
        let version = Version::parse("9.9.9").expect("version");

        // Not a gzip at all.
        let result = install(b"not an archive", &exe, version);
        assert!(
            matches!(result, Err(UpgradeFailed::Unpack(_))),
            "{result:?}"
        );

        // An archive whose leaf prints the wrong version.
        let src = dir.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("leaf"), "#!/bin/sh\necho leaf 0.0.1\n").expect("script");
        let tarball = dir.join("a.tar.gz");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&src)
            .arg("leaf")
            .status()
            .expect("tar runs");
        assert!(status.success());
        let archive = std::fs::read(&tarball).expect("archive");
        let result = install(&archive, &exe, version);
        assert!(
            matches!(result, Err(UpgradeFailed::Broken { .. })),
            "{result:?}"
        );
        assert_eq!(std::fs::read(&exe).expect("still there"), b"old");

        // One that answers with the right version replaces it.
        std::fs::write(src.join("leaf"), "#!/bin/sh\necho leaf 9.9.9\n").expect("script");
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&tarball)
            .arg("-C")
            .arg(&src)
            .arg("leaf")
            .status()
            .expect("tar runs");
        assert!(status.success());
        let archive = std::fs::read(&tarball).expect("archive");
        install(&archive, &exe, version).expect("installs");
        assert!(
            std::fs::read_to_string(&exe)
                .expect("new")
                .contains("9.9.9")
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".leaf-upgrade"))
            .collect();
        assert!(leftovers.is_empty(), "staged files are cleaned up");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
