//! Compiles Leaf's native pane helpers on macOS so they can be embedded in
//! the leaf binary: the WebKit browser (src/browser.swift) and the iOS
//! Simulator (src/simulator.swift), each with the shared pane code
//! (src/pane.swift). Elsewhere, or without Apple's Swift toolchain, an empty
//! helper is embedded and `leaf browser` or `leaf sim open` says so.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A helper: its output name, its own source, and what it's called in a warning.
const HELPERS: [(&str, &str, &str); 2] = [
    (
        "leaf-webkit-host",
        "src/browser.swift",
        "the WebKit browser helper",
    ),
    (
        "leaf-sim-host",
        "src/simulator.swift",
        "the iOS Simulator helper",
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=src/pane.swift");
    for (_, source, _) in HELPERS {
        println!("cargo:rerun-if-changed={source}");
    }
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo sets TARGET");

    let Some(arch) = target.strip_suffix("-apple-darwin") else {
        for (name, _, _) in HELPERS {
            fs::write(out.join(name), []).expect("write an empty helper");
        }
        return;
    };
    let arch = if arch == "aarch64" { "arm64" } else { arch };
    // The helpers use macOS 13 API, whatever Leaf itself targets.
    let triple = format!("{arch}-apple-macos13.0");
    let mut any_failed = false;
    for (name, source, what) in HELPERS {
        let helper = out.join(name);
        if compile(&triple, source, &helper, what) {
            continue;
        }
        any_failed = true;
        fs::write(&helper, []).expect("write an empty helper");
    }
    if any_failed {
        // Without a helper, try again once a toolchain appears: Cargo reruns
        // this script while a watched path is missing or has changed.
        println!("cargo:rerun-if-env-changed=DEVELOPER_DIR");
        for swiftc in [
            "/Library/Developer/CommandLineTools/usr/bin/swiftc",
            "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc",
        ] {
            println!("cargo:rerun-if-changed={swiftc}");
        }
    }
}

/// Compiles one helper; false (after a warning) when it couldn't be built.
fn compile(triple: &str, source: &str, helper: &Path, what: &str) -> bool {
    let result = Command::new("xcrun")
        .args([
            "swiftc",
            "-O",
            "-swift-version",
            "5",
            "-parse-as-library",
            "-target",
            triple,
        ])
        .args(["src/pane.swift", source])
        .arg("-o")
        .arg(helper)
        .env_remove("MACOSX_DEPLOYMENT_TARGET")
        .status();
    let failed = match &result {
        Ok(status) => !status.success(),
        Err(_) => true,
    };
    // A release must never ship without its helpers.
    if failed && env::var_os("CI").is_some() {
        panic!("could not compile {source} for {triple}: {result:?}");
    }
    match result {
        Ok(status) if status.success() => return true,
        Ok(status) => println!(
            "cargo:warning={what} failed to compile ({status}); it will be unavailable in this build"
        ),
        Err(error) => println!(
            "cargo:warning=Apple's Swift toolchain wasn't found ({error}); {what} will be unavailable. Install the Xcode Command Line Tools with `xcode-select --install`."
        ),
    }
    false
}
