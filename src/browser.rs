//! `leaf browser`: a WebKit page in a Rex pane, on macOS.
//!
//! Rex draws its blocks itself, so the page lives in a small native helper
//! (src/browser.swift, compiled by build.rs and embedded here) whose window
//! follows the pane. The helper is unpacked once per Leaf version into the
//! user's cache folder; nothing is installed in Applications.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

const WEBKIT_HOST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/leaf-webkit-host"));

/// FNV-1a, to name the unpacked helper after its contents.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The helper on disk, unpacking it the first time this build runs it.
fn helper() -> Result<PathBuf, String> {
    if WEBKIT_HOST.is_empty() {
        return Err("this leaf was built without the WebKit browser (Apple's Swift toolchain wasn't available). Install the Xcode Command Line Tools with `xcode-select --install`, then reinstall leaf.".into());
    }
    let home = std::env::var_os("HOME")
        .ok_or("HOME is unset, so there's nowhere to keep the browser helper")?;
    let dir = PathBuf::from(home)
        .join("Library/Caches/leaf")
        .join(format!("webkit-{:016x}", fingerprint(WEBKIT_HOST)));
    // A fixed name: WebKit keys cookies and site data by it, so they last
    // across Leaf updates.
    let executable = dir.join("leaf-webkit-host");
    if fs::metadata(&executable).is_ok_and(|meta| meta.len() == WEBKIT_HOST.len() as u64) {
        return Ok(executable);
    }
    fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let partial = dir.join(format!("leaf-webkit-host.{}", std::process::id()));
    fs::write(&partial, WEBKIT_HOST)
        .map_err(|e| format!("couldn't write {}: {e}", partial.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("couldn't make {} executable: {e}", partial.display()))?;
    }
    fs::rename(&partial, &executable)
        .map_err(|e| format!("couldn't place {}: {e}", executable.display()))?;
    // Helpers from earlier Leaf versions are no longer used. Removing them is
    // best effort: one left behind only takes a little disk space, and a
    // browser still running from it keeps working.
    if let Some(cache) = dir.parent()
        && let Ok(entries) = fs::read_dir(cache)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = path != dir
                && entry.file_name().to_string_lossy().starts_with("webkit-")
                && path.is_dir();
            if stale {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
    Ok(executable)
}

pub fn run(args: &[String]) -> ExitCode {
    if !cfg!(target_os = "macos") {
        eprintln!("leaf: the WebKit browser runs on macOS only.");
        return ExitCode::FAILURE;
    }
    if std::env::var_os("REX_SESSION").is_none() || std::env::var_os("REX_BLOCK").is_none() {
        eprintln!(
            "leaf: open the browser from a Rex pane, or with Leaf: Open Browser in Rex's command palette."
        );
        return ExitCode::FAILURE;
    }
    if args.len() > 1 {
        eprintln!("Usage: leaf browser [URL]");
        return ExitCode::from(2);
    }
    let executable = match helper() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("leaf: {error}");
            return ExitCode::FAILURE;
        }
    };
    let rex = match crate::agent::rex_program() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("leaf: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut command = Command::new(&executable);
    command.stdin(Stdio::null()).env("LEAF_REX", rex);
    if let Some(url) = args.first() {
        command.env("LEAF_BROWSER_URL", url);
    }
    match command.status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => {
            eprintln!("leaf: the browser stopped unexpectedly ({status}).");
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!(
                "leaf: couldn't start the browser at {} ({error}).",
                executable.display()
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn fingerprint_tells_builds_apart() {
        assert_eq!(fingerprint(b""), 0xcbf2_9ce4_8422_2325);
        assert_ne!(fingerprint(b"a"), fingerprint(b"b"));
    }
}
