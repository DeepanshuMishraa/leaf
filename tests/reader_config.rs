#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let root = std::env::temp_dir().join(format!(
            "leaf-config-{}-{}-{}",
            std::process::id(),
            stamp.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("config/leaf")).unwrap();
        Self(root)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_leaf"));
        command.current_dir(&self.0).env_clear();
        for (name, folder) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("PATH", "bin"),
        ] {
            command.env(name, self.0.join(folder));
        }
        command.args(["open", "missing.md"]);
        command
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn invalid_config_is_reported_before_document_loading() {
    let fixture = Fixture::new();
    let path = fixture.0.join("config/leaf/config.toml");
    for config in [
        "[reader]\nmax_width = 0",
        "[readre]",
        "[reader]\nmax_wdth = 90",
        "[reader",
    ] {
        fs::write(&path, config).unwrap();
        let output = fixture.command().output().unwrap();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success(), "accepted {config:?}");
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("Could not load settings"), "{error}");
        assert!(!error.contains("Could not read missing.md"), "{error}");
    }
}

#[test]
fn home_config_is_used_when_xdg_is_unset_or_empty() {
    let fixture = Fixture::new();
    let path = fixture.0.join("home/.config/leaf/config.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "[reader]\nmax_width = 0").unwrap();
    for empty in [false, true] {
        let mut command = fixture.command();
        command.env_remove("XDG_CONFIG_HOME");
        if empty {
            command.env("XDG_CONFIG_HOME", "");
        }
        let output = command.output().unwrap();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success());
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("reader.max_width"), "{error}");
    }
}
