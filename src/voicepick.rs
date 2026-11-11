//! `leaf voices`: pick who reads aloud, in a Rex-style list. The built-in
//! voices show at once; Microsoft's full list replaces them when it arrives.

use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use crate::aloud;
use crate::edge::{self, Voice};
use crate::popover::{self, Edit, Row, Section};

fn listing(
    voices: &[Voice],
    typed: &str,
    current: &str,
    complete: bool,
) -> (Vec<Section>, Vec<String>) {
    let ranked = aloud::rank(voices, typed);
    let names = ranked.iter().map(|v| v.name.clone()).collect();
    let rows = ranked
        .iter()
        .map(|voice| Row {
            icon: "",
            label: voice.short().to_owned(),
            detail: format!("{} · {}  ", voice.language, voice.gender),
            checked: voice.name == current,
        })
        .collect();
    let title = if complete {
        format!("{} voices", voices.len())
    } else {
        "Built-in voices · loading every voice…".to_owned()
    };
    (vec![Section { title, rows }], names)
}

/// Shows the voices until one is picked, or `None` when closed.
pub fn pick(current: &str) -> io::Result<Option<String>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(edge::voices());
    });
    let mut voices = edge::built_in();
    let mut complete = false;
    let mut typed = String::new();
    // Starts on the voice reading now.
    let mut selected = voices.iter().position(|v| v.name == current).unwrap_or(0);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle("Voice"));
    let result = (|| loop {
        if !complete && let Ok(listed) = receive.try_recv() {
            complete = true;
            if let Ok(listed) = listed
                && !listed.is_empty()
            {
                let at = typed
                    .is_empty()
                    .then(|| voices.get(selected).map(|v| v.name.clone()))
                    .flatten();
                voices = listed;
                if let Some(name) = at {
                    selected = voices.iter().position(|v| v.name == name).unwrap_or(0);
                }
            }
        }
        let (sections, names) = listing(&voices, &typed, current, complete);
        selected = selected.min(names.len().saturating_sub(1));
        terminal.draw(|f| {
            popover::draw(
                f,
                &popover::View {
                    chip: "Voice",
                    typed: &typed,
                    placeholder: "Search by name, language or gender…",
                    sections: &sections,
                    selected,
                    empty: "No voice matches. Try a name, language or gender.",
                    footer: "enter read in this voice · esc close",
                },
            )
        })?;
        let wait = if complete {
            Duration::from_secs(60)
        } else {
            Duration::from_millis(100)
        };
        if !event::poll(wait)? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match popover::edit(
            key.code,
            key.modifiers,
            &mut typed,
            &mut selected,
            names.len(),
        ) {
            Edit::Quit => return Ok(None),
            Edit::Choose => {
                if let Some(name) = names.get(selected) {
                    return Ok(Some(name.clone()));
                }
            }
            Edit::Complete | Edit::Typed | Edit::Moved | Edit::Nothing => {}
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_voice_reading_now_is_checked_and_search_narrows() {
        let voices = edge::built_in();
        let (sections, names) = listing(&voices, "", "en-GB-RyanNeural", true);
        assert_eq!(names.len(), voices.len());
        let checked: Vec<_> = sections[0]
            .rows
            .iter()
            .filter(|r| r.checked)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(checked, ["Ryan"]);
        let (_, names) = listing(&voices, "kingdom male", "", true);
        assert_eq!(names, ["en-GB-RyanNeural"]);
    }
}
