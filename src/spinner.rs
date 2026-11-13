//! A spinner on stderr for slow commands such as `leaf upgrade`: each step
//! spins while it runs, then leaves `✓ step` or `✗ step` behind. When stderr
//! isn't a terminal only those finished lines are written.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(80);

/// What's spinning, shared with the thread that draws it.
struct Shared {
    label: String,
    /// Text after the label that changes as the step goes, such as a size.
    detail: Mutex<String>,
    stop: AtomicBool,
}

pub struct Spinner {
    shared: Arc<Shared>,
    drawing: Option<JoinHandle<()>>,
}

impl Spinner {
    fn start(label: &str) -> Self {
        let shared = Arc::new(Shared {
            label: label.to_owned(),
            detail: Mutex::new(String::new()),
            stop: AtomicBool::new(false),
        });
        let drawing = io::stderr().is_terminal().then(|| {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let mut frame = 0;
                while !shared.stop.load(Ordering::Relaxed) {
                    let line = format!("{} {}", FRAMES[frame % FRAMES.len()], shared.line());
                    // A stderr that can't be written to only loses the animation.
                    let _ =
                        write!(io::stderr(), "\r\x1b[2K{line}").and_then(|_| io::stderr().flush());
                    frame += 1;
                    thread::sleep(TICK);
                }
            })
        });
        Spinner { shared, drawing }
    }

    /// Shows `detail` after the label, such as how much is downloaded.
    pub fn detail(&self, detail: String) {
        if let Ok(mut current) = self.shared.detail.lock() {
            *current = detail;
        }
    }

    fn finish(mut self, ok: bool) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let tty = match self.drawing.take() {
            Some(drawing) => {
                let _ = drawing.join();
                true
            }
            None => false,
        };
        let mark = if ok { "✓" } else { "✗" };
        let clear = if tty { "\r\x1b[2K" } else { "" };
        let _ = writeln!(io::stderr(), "{clear}{mark} {}", self.shared.line());
    }
}

impl Shared {
    fn line(&self) -> String {
        let label = &self.label;
        let detail = self.detail.lock().map(|d| d.clone()).unwrap_or_default();
        if detail.is_empty() {
            label.clone()
        } else {
            format!("{label} {detail}")
        }
    }
}

/// Runs `work` with a spinner labelled `label`, and leaves a ✓ or ✗ line.
pub fn step<T, E>(label: &str, work: impl FnOnce(&Spinner) -> Result<T, E>) -> Result<T, E> {
    let spinner = Spinner::start(label);
    let result = work(&spinner);
    spinner.finish(result.is_ok());
    result
}

/// `bytes` as megabytes, one decimal: `3.2 MB`.
pub fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_returns_what_its_work_did() {
        assert_eq!(step("adding", |_| Ok::<_, ()>(2 + 2)), Ok(4));
        assert_eq!(step("failing", |_| Err::<(), _>("no")), Err("no"));
    }

    #[test]
    fn sizes_read_in_megabytes() {
        assert_eq!(megabytes(3_210_000), "3.2 MB");
        assert_eq!(megabytes(0), "0.0 MB");
    }
}
