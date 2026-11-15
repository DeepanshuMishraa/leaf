#![cfg(unix)]

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OLD: &str = "# Plan\n\nOLD-SNAPSHOT-CONTENTS\n";
const NEW: &str = "# Plan\n\nNEW-SNAPSHOT-CONTENTS\n";
const LIVE: &str = "# Plan\n\nLIVE-SNAPSHOT-CONTENTS\n";

struct Fixture {
    root: PathBuf,
    history: Option<PathBuf>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let root = std::env::temp_dir().join(format!(
            "leaf-plan-round-{}-{}-{}",
            std::process::id(),
            stamp.as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            root,
            history: None,
        };
        for name in ["home", "data", "cache", "config", "state", "runtime"] {
            fs::create_dir(fixture.root.join(name)).unwrap();
        }
        fs::write(fixture.root.join("plan.md"), OLD).unwrap();
        fixture
    }

    fn find_history(&mut self) -> PathBuf {
        let rounds = self.root.join("data/leaf/margin/rounds");
        let entries: Vec<_> = fs::read_dir(rounds)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "one plan's history should be recorded");
        self.history = Some(entries[0].clone());
        entries[0].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(history) = &self.history {
            let _ = fs::set_permissions(history, fs::Permissions::from_mode(0o700));
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Review {
    child: Child,
    terminal: File,
    output: Vec<u8>,
}

impl Review {
    fn open(fixture: &Fixture) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0,
            "openpty: {}",
            io::Error::last_os_error()
        );
        let terminal = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        for fd in [terminal.as_raw_fd(), slave.as_raw_fd()] {
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            -1
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_leaf"));
        command
            .args(["plan", "--print", "plan.md"])
            .current_dir(&fixture.root)
            .env_clear()
            .env("HOME", fixture.root.join("home"))
            .env("XDG_DATA_HOME", fixture.root.join("data"))
            .env("XDG_CACHE_HOME", fixture.root.join("cache"))
            .env("XDG_CONFIG_HOME", fixture.root.join("config"))
            .env("XDG_STATE_HOME", fixture.root.join("state"))
            .env("XDG_RUNTIME_DIR", fixture.root.join("runtime"))
            .env("TERM", "xterm-256color")
            .env("LEAF_THEME", "dark")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1
                    || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            terminal,
            output: Vec::new(),
        }
    }

    fn drain(&mut self) {
        let mut buffer = [0; 8192];
        loop {
            match self.terminal.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => self.output.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(error) => panic!("reading PTY: {error}"),
            }
        }
    }

    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if self
                .output
                .windows(marker.len())
                .any(|part| part == marker.as_bytes())
            {
                return;
            }
            let status = self.child.try_wait().unwrap();
            assert!(
                status.is_none() && Instant::now() < deadline,
                "missing {marker:?}; exit={status:?}; output={:?}",
                String::from_utf8_lossy(&self.output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self, keys: &[u8]) -> String {
        self.terminal.write_all(keys).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.drain();
                assert!(status.success(), "leaf exited {status}: {:?}", self.output);
                return String::from_utf8_lossy(&self.output).into_owned();
            }
            assert!(
                Instant::now() < deadline,
                "leaf did not exit; output={:?}",
                String::from_utf8_lossy(&self.output)
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Review {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn sending_after_round_recording_fails_preserves_the_existing_snapshot() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: root bypasses the mode-0300 directory listing restriction");
        return;
    }
    let mut fixture = Fixture::new();
    let mut review = Review::open(&fixture);
    review.until("OLD-SNAPSHOT-CONTENTS");
    assert!(review.finish(b"q").contains("closed without a verdict"));
    let history = fixture.find_history();
    let snapshot = history.join("1.md");
    assert_eq!(fs::read_to_string(&snapshot).unwrap(), OLD);

    fs::write(fixture.root.join("plan.md"), NEW).unwrap();
    fs::set_permissions(&history, fs::Permissions::from_mode(0o300)).unwrap();
    let error = fs::read_dir(&history).expect_err("history must not be listable");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    fs::OpenOptions::new().write(true).open(&snapshot).unwrap();

    let mut review = Review::open(&fixture);
    review.until("NEW-SNAPSHOT-CONTENTS");
    assert!(review.finish(b"Sa").contains("Approved. Go ahead."));
    assert_eq!(
        fs::read_to_string(&snapshot).unwrap(),
        OLD,
        "a failed round record must not overwrite the earlier snapshot on Send"
    );
}

#[test]
fn recorded_rounds_keep_old_snapshots_and_save_the_live_reviewed_source() {
    let mut fixture = Fixture::new();
    let mut review = Review::open(&fixture);
    review.until("OLD-SNAPSHOT-CONTENTS");
    review.finish(b"q");
    let history = fixture.find_history();

    fs::write(fixture.root.join("plan.md"), NEW).unwrap();
    let mut review = Review::open(&fixture);
    review.until("NEW-SNAPSHOT-CONTENTS");
    assert_eq!(fs::read_to_string(history.join("2.md")).unwrap(), NEW);
    fs::write(fixture.root.join("plan.md"), LIVE).unwrap();
    review.until("LIVE-SNAPSHOT-CONTENTS");
    assert!(review.finish(b"Sa").contains("Approved. Go ahead."));
    assert_eq!(fs::read_to_string(history.join("1.md")).unwrap(), OLD);
    assert_eq!(fs::read_to_string(history.join("2.md")).unwrap(), LIVE);
    assert!(!history.join("3.md").exists());
}
