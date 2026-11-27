//! leaf's native pane helpers (the WebKit browser and the iOS Simulator) are
//! compiled by build.rs and embedded in the leaf binary. Each is unpacked
//! once per leaf version into the user's cache folder, with the files it
//! reads beside it; nothing is installed in Applications.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// One file of an unpacked helper.
pub struct Part<'a> {
    pub name: &'a str,
    pub bytes: &'a [u8],
    pub mode: u32,
}

/// FNV-1a, to name an unpacked helper after its contents.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The folder holding `parts`, unpacking them the first time this build
/// runs them: `~/Library/Caches/leaf/<prefix>-<fingerprint>`. Folders left
/// by earlier versions of the same helper are removed.
pub fn unpack(prefix: &str, what: &str, parts: &[Part]) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| format!("HOME is unset, so there's nowhere to keep the {what} helper"))?;
    let combined = parts.iter().enumerate().fold(0u64, |hash, (index, part)| {
        hash ^ fingerprint(part.bytes).rotate_left(index as u32)
    });
    let dir = PathBuf::from(home)
        .join("Library/Caches/leaf")
        .join(format!("{prefix}-{combined:016x}"));
    let unpacked = |part: &Part| {
        fs::metadata(dir.join(part.name)).is_ok_and(|meta| meta.len() == part.bytes.len() as u64)
    };
    if parts.iter().all(unpacked) {
        return Ok(dir);
    }
    fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // Written beside, then renamed into place, so a second pane opening at
    // the same moment never runs a half-written file.
    for part in parts {
        let target = dir.join(part.name);
        let partial = target.with_extension(format!("{}.part", std::process::id()));
        let placed = (|| {
            fs::write(&partial, part.bytes)
                .map_err(|e| format!("couldn't write {}: {e}", partial.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&partial, fs::Permissions::from_mode(part.mode)).map_err(
                    |e| format!("couldn't set permissions on {}: {e}", partial.display()),
                )?;
            }
            fs::rename(&partial, &target)
                .map_err(|e| format!("couldn't place {}: {e}", target.display()))
        })();
        if placed.is_err() {
            let _ = fs::remove_file(&partial);
        }
        placed?;
    }
    // Removing old helpers is best effort: one left behind only takes a
    // little disk space. A folder a pane is still running from is kept, as
    // the pane may still read files from it (the browser's scripts).
    if let Some(cache) = dir.parent()
        && let Ok(entries) = fs::read_dir(cache)
    {
        let own = format!("{prefix}-");
        for entry in entries.flatten() {
            let path = entry.path();
            if path != dir
                && entry.file_name().to_string_lossy().starts_with(&own)
                && path.is_dir()
                && !in_use(&path)
            {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
    Ok(dir)
}

/// True when a running process was started from inside `folder` (or when
/// that can't be told, to be safe).
fn in_use(folder: &std::path::Path) -> bool {
    let pattern = format!("^{}/", regex::escape(&folder.to_string_lossy()));
    Command::new("/usr/bin/pgrep")
        .args(["-f", &pattern])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_or(true, |status| status.code() != Some(1))
}

/// This Mac's macOS version as (major, minor), when it can be read.
pub fn macos_version() -> Option<(u32, u32)> {
    let output = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |minor| minor.parse().ok())?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::{fingerprint, parse_version};

    #[test]
    fn macos_versions_parse() {
        assert_eq!(parse_version("27.0\n"), Some((27, 0)));
        assert_eq!(parse_version("12.7.6"), Some((12, 7)));
        assert_eq!(parse_version("14"), Some((14, 0)));
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("x.y"), None);
    }

    #[test]
    fn fingerprint_tells_builds_apart() {
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fingerprint(b"a"), fingerprint(b"b"));
    }
}
