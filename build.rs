//! Compiles Leaf's WebKit pane helper (src/browser.swift) on macOS so it can
//! be embedded in the leaf binary. Elsewhere, or without Apple's Swift
//! toolchain, an empty helper is embedded and `leaf browser` says so.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/browser.swift");
    println!("cargo:rerun-if-changed=src/browser/main.swift");
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let helper = out.join("leaf-webkit-host");
    let target = env::var("TARGET").expect("Cargo sets TARGET");

    if let Some(arch) = target.strip_suffix("-apple-darwin") {
        let arch = if arch == "aarch64" { "arm64" } else { arch };
        // The helper uses macOS 13 WebKit API, whatever Leaf itself targets.
        let triple = format!("{arch}-apple-macos13.0");
        let result = Command::new("xcrun")
            .args(["swiftc", "-O", "-swift-version", "5", "-target", &triple])
            .args(["src/browser.swift", "src/browser/main.swift"])
            .arg("-o")
            .arg(&helper)
            .env_remove("MACOSX_DEPLOYMENT_TARGET")
            .status();
        let failed = match &result {
            Ok(status) => !status.success(),
            Err(_) => true,
        };
        // A release must never ship without the browser.
        if failed && env::var_os("CI").is_some() {
            panic!("could not compile src/browser.swift for {triple}: {result:?}");
        }
        match result {
            Ok(status) if status.success() => return,
            Ok(status) => println!(
                "cargo:warning=the WebKit browser helper failed to compile ({status}); `leaf browser` will be unavailable"
            ),
            Err(error) => println!(
                "cargo:warning=Apple's Swift toolchain wasn't found ({error}); `leaf browser` will be unavailable. Install the Xcode Command Line Tools with `xcode-select --install`."
            ),
        }
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
    fs::write(&helper, []).expect("write the empty WebKit helper");
}
