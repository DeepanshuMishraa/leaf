//! Compiles Leaf's native pane helpers on macOS so they can be embedded in
//! the leaf binary: the WebKit browser (src/browser.swift and its entry
//! point, src/browser/main.swift) and the iOS Simulator (src/simulator.swift
//! with the pane code in src/pane.swift). Elsewhere, or without Apple's Swift
//! toolchain, an empty helper is embedded and `leaf browser` or `leaf sim
//! open` says so.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A helper Leaf embeds.
struct Helper {
    /// Its file name in OUT_DIR.
    name: &'static str,
    sources: &'static [&'static str],
    /// True when no source is a main.swift, so the entry point is `@main`.
    library: bool,
    /// What a warning calls it.
    what: &'static str,
}

const HELPERS: [Helper; 2] = [
    Helper {
        name: "leaf-webkit-host",
        sources: &["src/browser.swift", "src/browser/main.swift"],
        library: false,
        what: "the WebKit browser helper",
    },
    Helper {
        name: "leaf-sim-host",
        sources: &["src/pane.swift", "src/simulator.swift"],
        library: true,
        what: "the iOS Simulator helper",
    },
];

fn main() {
    for helper in &HELPERS {
        for source in helper.sources {
            println!("cargo:rerun-if-changed={source}");
        }
    }
    println!("cargo:rerun-if-changed={DUO_HINGE_SOURCE}");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let target = env::var("TARGET").expect("Cargo sets TARGET");

    let Some(arch) = target.strip_suffix("-apple-darwin") else {
        for helper in &HELPERS {
            fs::write(out.join(helper.name), []).expect("write an empty helper");
        }
        fs::write(out.join(DUO_HINGE), []).expect("write an empty helper");
        return;
    };
    let arch = if arch == "aarch64" { "arm64" } else { arch };
    let hinge = out.join(DUO_HINGE);
    if !compile_duo_hinge(arch, &hinge) {
        fs::write(&hinge, []).expect("write an empty helper");
    }
    // The helpers use macOS 13 API, whatever Leaf itself targets.
    let triple = format!("{arch}-apple-macos13.0");
    let mut any_failed = false;
    for helper in &HELPERS {
        let output = out.join(helper.name);
        if compile(&triple, helper, &output) {
            continue;
        }
        any_failed = true;
        fs::write(&output, []).expect("write an empty helper");
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

/// The iPhone Duo's hinge helper: an iOS Simulator program the simulator pane
/// runs inside the device (`simctl spawn`), so it's built for the simulator
/// SDK, not for macOS.
const DUO_HINGE: &str = "leaf-duo-hinge";
const DUO_HINGE_SOURCE: &str = "src/duo-hinge.m";

/// Compiles the hinge helper for the simulator on this Mac's architecture;
/// false (after a warning) when it couldn't be built. The pane then still
/// shows the Duo, but its iOS stays on the cover screen.
fn compile_duo_hinge(arch: &str, output: &Path) -> bool {
    let result = Command::new("xcrun")
        .args(["-sdk", "iphonesimulator", "clang", "-O2", "-fobjc-arc", "-target"])
        .arg(format!("{arch}-apple-ios17.0-simulator"))
        .args(["-framework", "Foundation", "-framework", "IOKit", DUO_HINGE_SOURCE, "-o"])
        .arg(output)
        .env_remove("MACOSX_DEPLOYMENT_TARGET")
        .env_remove("IPHONEOS_DEPLOYMENT_TARGET")
        .status();
    match result {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("cargo:warning=the iPhone Duo hinge helper failed to compile ({status}); the Duo's iOS will stay on its cover screen in this build");
            false
        }
        Err(error) => {
            println!("cargo:warning=xcrun wasn't found ({error}); the iPhone Duo hinge helper will be unavailable. Install Xcode with the iOS Simulator SDK.");
            false
        }
    }
}

/// Compiles one helper; false (after a warning) when it couldn't be built.
fn compile(triple: &str, helper: &Helper, output: &Path) -> bool {
    let what = helper.what;
    let mut command = Command::new("xcrun");
    command.args(["swiftc", "-O", "-swift-version", "5", "-target", triple]);
    if helper.library {
        command.arg("-parse-as-library");
    }
    let result = command
        .args(helper.sources)
        .arg("-o")
        .arg(output)
        .env_remove("MACOSX_DEPLOYMENT_TARGET")
        .status();
    let failed = match &result {
        Ok(status) => !status.success(),
        Err(_) => true,
    };
    // A release must never ship without its helpers.
    if failed && env::var_os("CI").is_some() {
        panic!(
            "could not compile {} for {triple}: {result:?}",
            helper.sources.join(" ")
        );
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
