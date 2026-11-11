//! Commands from Rex's command palette to a document open in a Rex pane.
//!
//! The reader announces itself with a program status record (OSC 7501) whose
//! id is [`RECORD`]. The plugin finds a pane holding that record, owned by the
//! process in front, and types F12, one command line, and enter into it. Both
//! go through Rex itself, so they reach a reader on a remote host as well as
//! one on this machine. F12 starts the command before any dialog can take the
//! keys as typing.

use std::io::Write;

use base64::Engine;
use crossterm::event::KeyCode;

/// The key that starts a command from the plugin.
pub const WAKE: KeyCode = KeyCode::F(12);

/// The id of the record that says a document is open in this pane.
pub const RECORD: &str = "leaf/reader";

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

/// The record saying `title` is open here.
pub fn announcement(title: &str) -> String {
    let title: String = title
        .chars()
        .filter(|c| !c.is_control())
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= 190).then_some(c)
        })
        .collect();
    let title = base64::engine::general_purpose::STANDARD.encode(title);
    format!("\x1b]7501;id={RECORD}:state=idle:app=leaf:title={title}\x1b\\")
}

pub fn withdrawal() -> String {
    format!("\x1b]7501;id={RECORD}:state=clear\x1b\\")
}

/// Tells Rex a document is open in this pane, when leaf runs inside Rex.
pub fn announce(title: &str) -> bool {
    inside_rex() && emit(&announcement(title))
}

pub fn withdraw() {
    if inside_rex() {
        emit(&withdrawal());
    }
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
    fn the_record_names_the_document_within_rex_limits() {
        let long = "é".repeat(500);
        let sequence = announcement(&format!("A\nbook {long}"));
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
        assert_eq!(withdrawal(), "\x1b]7501;id=leaf/reader:state=clear\x1b\\");
    }
}
