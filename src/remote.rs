//! Commands from Rex's command palette to a document open in a Rex pane.
//!
//! The reader announces itself with a program status record (OSC 7501) whose
//! id is [`Kind::record`]. The plugin finds a pane holding that record, owned by the
//! process in front, and types F12, one command line, and enter into it. Both
//! go through Rex itself, so they reach a reader on a remote host as well as
//! one on this machine. F12 starts the command before any dialog can take the
//! keys as typing.

use std::io::Write;

use base64::Engine;
use crossterm::event::KeyCode;

/// The key that starts a command from the plugin.
pub const WAKE: KeyCode = KeyCode::F(12);

/// What leaf shows in a pane, which decides the palette actions offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A document: asking, reading aloud, voices.
    Document,
    /// A diff: asking the agent about lines.
    Diff,
}

impl Kind {
    /// The id of the program status record that says this is open in a pane.
    pub fn record(self) -> &'static str {
        match self {
            Kind::Document => "leaf/reader",
            Kind::Diff => "leaf/diff",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Document => "document",
            Kind::Diff => "diff",
        }
    }

    fn parse(name: &str) -> Option<Kind> {
        match name.trim() {
            "document" | "" => Some(Kind::Document),
            "diff" => Some(Kind::Diff),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Read aloud from the screen, or pause and carry on.
    Play,
    Stop,
    Faster,
    Slower,
    Next,
    Previous,
    /// Open the reader's voice picker.
    Voices,
    /// Close the reader, so another document can take its pane.
    Quit,
    /// Read in this voice from now on.
    Voice(String),
    /// Send the selection, with the lines around it, to an agent pane.
    /// Empty asks the reader to type a question.
    Ask(String),
}

impl Command {
    pub fn parse(line: &str) -> Result<Command, String> {
        let line = line.trim();
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match (verb, rest.is_empty()) {
            ("play", true) => Ok(Command::Play),
            ("stop", true) => Ok(Command::Stop),
            ("faster", true) => Ok(Command::Faster),
            ("slower", true) => Ok(Command::Slower),
            ("next", true) => Ok(Command::Next),
            ("previous", true) => Ok(Command::Previous),
            ("voices", true) => Ok(Command::Voices),
            ("quit", true) => Ok(Command::Quit),
            ("voice", false) if is_voice_name(rest) => Ok(Command::Voice(rest.to_owned())),
            ("ask", _) => Ok(Command::Ask(rest.to_owned())),
            _ => Err(format!(
                "Rex sent \"{line}\", which this leaf doesn't know; update it with cargo install leaf-reader"
            )),
        }
    }
}

/// Voice names are like `en-US-AriaNeural`: letters, digits and dashes.
pub fn is_voice_name(name: &str) -> bool {
    name.len() <= 80
        && name.contains('-')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// What the plugin types as a command line grows to at most this.
pub const LONGEST: usize = 2000;

/// The record saying `title` is open here, as a `kind`.
pub fn announcement(kind: Kind, title: &str) -> String {
    let title: String = title
        .chars()
        .filter(|c| !c.is_control())
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= 190).then_some(c)
        })
        .collect();
    let title = base64::engine::general_purpose::STANDARD.encode(title);
    format!(
        "\x1b]7501;id={}:state=idle:app=leaf:title={title}\x1b\\",
        kind.record()
    )
}

pub fn withdrawal(kind: Kind) -> String {
    format!("\x1b]7501;id={}:state=clear\x1b\\", kind.record())
}

/// Tells Rex a `kind` is open in this pane, when leaf runs inside Rex.
/// Rex's palette offers the matching actions only while one is, so the
/// plugin is reloaded when the kinds open change.
pub fn announce(kind: Kind, title: &str) -> bool {
    if !inside_rex() {
        return false;
    }
    let readers = Readers::here();
    let before = readers.open();
    readers.add(std::process::id(), kind);
    if readers.open() != before {
        reload_rex();
    }
    emit(&announcement(kind, title))
}

pub fn withdraw(kind: Kind) {
    if inside_rex() {
        emit(&withdrawal(kind));
        let readers = Readers::here();
        let before = readers.open();
        readers.remove(std::process::id());
        // Waits: the pane closes as leaf exits, and its hangup would stop a
        // reload still running in the background.
        if readers.open() != before {
            refresh_palettes();
        }
    }
}

/// What leaf shows on this machine, one file per process id holding its kind.
pub struct Readers {
    dir: std::path::PathBuf,
}

impl Readers {
    pub fn here() -> Readers {
        Readers {
            dir: crate::progress::default_path().with_file_name("readers"),
        }
    }

    fn add(&self, pid: u32, kind: Kind) {
        let _ = std::fs::create_dir_all(&self.dir);
        let _ = std::fs::write(self.dir.join(pid.to_string()), kind.name());
    }

    fn remove(&self, pid: u32) {
        let _ = std::fs::remove_file(self.dir.join(pid.to_string()));
    }

    /// The kinds still open, each once, forgetting processes that died.
    pub fn open(&self) -> Vec<Kind> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut kinds = Vec::new();
        for entry in entries.flatten() {
            let pid = entry.file_name().to_string_lossy().parse::<i32>().ok();
            let kind = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|text| Kind::parse(&text));
            match (pid, kind) {
                (Some(pid), Some(kind)) if alive(pid) => kinds.push(kind),
                _ => {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        kinds.sort();
        kinds.dedup();
        kinds
    }
}

fn alive(pid: i32) -> bool {
    // Signal 0 checks the process exists without touching it.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Re-runs the plugin's setup in the background; a reader never waits on it.
fn reload_rex() {
    std::thread::spawn(refresh_palettes);
}

/// Loads Rex's config again so the palette offers the right actions,
/// quietly: see [`crate::plugin::note_open`].
fn refresh_palettes() {
    let _ = crate::plugin::note_open(&state());
}

/// What leaf shows on this machine and the look it's in, as the plugin reads
/// it: `document diff dither`, or `idle classic`.
pub fn state() -> String {
    let open: Vec<&str> = Readers::here().open().into_iter().map(Kind::name).collect();
    let open = if open.is_empty() {
        "idle".to_owned()
    } else {
        open.join(" ")
    };
    let ui = crate::progress::Store::open(&crate::progress::default_path())
        .ok()
        .and_then(|store| store.load_ui().ok())
        .unwrap_or_default();
    format!("{open} {}", ui.as_str())
}

fn inside_rex() -> bool {
    std::env::var("REX_SESSION").is_ok_and(|s| !s.trim().is_empty())
}

fn emit(sequence: &str) -> bool {
    let mut out = std::io::stdout();
    out.write_all(sequence.as_bytes())
        .and_then(|_| out.flush())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_parsed_and_unknown_ones_explained() {
        assert_eq!(Command::parse("play\n"), Ok(Command::Play));
        assert_eq!(
            Command::parse("voice en-GB-RyanNeural"),
            Ok(Command::Voice("en-GB-RyanNeural".into()))
        );
        assert_eq!(
            Command::parse("ask what does this mean?"),
            Ok(Command::Ask("what does this mean?".into()))
        );
        assert_eq!(Command::parse("ask"), Ok(Command::Ask(String::new())));
        assert!(Command::parse("voice").is_err());
        assert!(Command::parse("voice ../../etc").is_err());
        assert!(
            Command::parse("dance")
                .unwrap_err()
                .contains("cargo install leaf-reader")
        );
    }

    #[test]
    fn only_running_readers_count_as_open() {
        let dir = std::env::temp_dir().join(format!("leaf-readers-{}", std::process::id()));
        let readers = Readers { dir: dir.clone() };
        assert!(readers.open().is_empty());
        readers.add(std::process::id(), Kind::Diff);
        assert_eq!(readers.open(), [Kind::Diff]);
        // A pid no process has: forgotten on the next look.
        readers.remove(std::process::id());
        readers.add(i32::MAX as u32 - 7, Kind::Document);
        assert!(readers.open().is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_record_names_the_document_within_rex_limits() {
        let long = "é".repeat(500);
        let sequence = announcement(Kind::Document, &format!("A\nbook {long}"));
        assert!(sequence.starts_with("\x1b]7501;id=leaf/reader:state=idle:app=leaf:title="));
        let title = sequence
            .split("title=")
            .nth(1)
            .and_then(|rest| rest.strip_suffix("\x1b\\"))
            .expect("title");
        assert!(title.len() <= 256, "{}", title.len());
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(title)
            .expect("base64");
        let decoded = String::from_utf8(decoded).expect("utf8");
        assert!(decoded.starts_with("Abook "));
        assert_eq!(
            withdrawal(Kind::Document),
            "\x1b]7501;id=leaf/reader:state=clear\x1b\\"
        );
        assert!(announcement(Kind::Diff, "x").contains("id=leaf/diff:"));
    }
}
