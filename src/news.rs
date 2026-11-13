//! "What's new": after an update, the first leaf screen opened (a reader, a
//! diff, a pull request or the home screen) shows the notes for the version
//! now running in a dialog, once, until OK. The notes are this version's
//! section of CHANGELOG.md.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::art::{self, Ink};
use crate::progress::Store;
use crate::render::DIM;

const CHANGELOG: &str = include_str!("../CHANGELOG.md");
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Widest the dialog gets.
const WIDTH: u16 = 64;
const OK: &str = "  OK  ";

pub struct News {
    version: &'static str,
    notes: Vec<&'static str>,
    /// Where OK was last drawn, so a click on it closes the dialog.
    ok: Rect,
}

impl News {
    /// This version's notes, when no leaf has shown them since the update.
    /// They count as seen once returned, so a second leaf pane doesn't show
    /// them too. A store that can't be read or saved shows nothing: the notes
    /// aren't worth an error, and showing them on every start would be worse.
    pub fn unseen() -> Option<Self> {
        Self::due(&store()?, VERSION, CHANGELOG)
    }

    fn due(store: &Store, version: &'static str, changelog: &'static str) -> Option<Self> {
        let seen = store.load_seen_version().ok()?;
        if seen.as_deref() == Some(version) {
            return None;
        }
        // A fresh install has no update to catch up on.
        let updated = seen.is_some() || store.used_before().ok()?;
        store.save_seen_version(version).ok()?;
        let notes = notes(changelog, version);
        (updated && !notes.is_empty()).then_some(News {
            version,
            notes,
            ok: Rect::default(),
        })
    }

    /// Takes every key while the notes are up; Enter, Esc, space, `o` or `q`
    /// closes them. True when the key was taken.
    pub fn key(news: &mut Option<Self>, key: KeyEvent) -> bool {
        if news.is_none() {
            return false;
        }
        let closes = matches!(
            key.code,
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char(' ' | 'o' | 'q')
        );
        if key.kind != KeyEventKind::Release && closes {
            *news = None;
        }
        true
    }

    /// Takes every click while the notes are up; one on OK closes them. True
    /// when the event was taken.
    pub fn mouse(news: &mut Option<Self>, mouse: MouseEvent) -> bool {
        let Some(open) = news else {
            return false;
        };
        let on_ok = open.ok.contains((mouse.column, mouse.row).into());
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) && on_ok {
            *news = None;
        }
        true
    }

    /// Draws the dialog centered over `f`, returning where it went so images
    /// under it can be taken away.
    pub fn draw(&mut self, f: &mut Frame, ink: Option<Ink>) -> Rect {
        let area = f.area();
        let width = WIDTH.min(area.width.saturating_sub(4));
        // Borders and two columns of padding on each side.
        let text = width.saturating_sub(6).max(1) as usize;
        let mut lines: Vec<Line> = self
            .notes
            .iter()
            .flat_map(|note| bullet(note, text))
            .collect();
        lines.push(Line::default());
        let height = (lines.len() as u16 + 5).min(area.height);
        let rect = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        f.render_widget(Clear, rect);
        if let Some(ink) = ink {
            art::surface(f.buffer_mut(), rect, ink);
            art::shadow(f.buffer_mut(), rect, ink);
        }
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(DIM))
            .title(
                Line::from(format!(" What's new in leaf {} ", self.version))
                    .style(Style::default().add_modifier(Modifier::BOLD))
                    .centered(),
            );
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let body = Rect {
            x: inner.x + 2,
            y: inner.y + 1,
            width: inner.width.saturating_sub(4),
            height: inner.height.saturating_sub(2),
        };
        f.render_widget(Paragraph::new(lines), body);
        let ok_width = OK.width() as u16;
        self.ok = Rect {
            x: inner.x + inner.width.saturating_sub(ok_width) / 2,
            y: inner.bottom().saturating_sub(2),
            width: ok_width.min(inner.width),
            height: 1,
        }
        .intersection(inner);
        f.render_widget(
            Paragraph::new(OK).style(
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            ),
            self.ok,
        );
        rect
    }
}

/// The `- ` lines under `## <version>` in the changelog.
fn notes(changelog: &'static str, version: &str) -> Vec<&'static str> {
    changelog
        .lines()
        .skip_while(|line| line.strip_prefix("## ").map(str::trim) != Some(version))
        .skip(1)
        .take_while(|line| !line.starts_with("## "))
        .filter_map(|line| line.strip_prefix("- "))
        .map(str::trim)
        .collect()
}

/// A note as a bullet wrapped to `width` columns, `code` in bold.
fn bullet(note: &str, width: usize) -> Vec<Line<'static>> {
    let words = note.split('`').enumerate().flat_map(|(i, part)| {
        let style = if i % 2 == 1 {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        part.split_whitespace().map(move |word| (word, style))
    });
    let room = width.saturating_sub(2).max(1);
    let mut lines = vec![vec![Span::styled("• ", Style::default().fg(DIM))]];
    let mut used = 0;
    for (word, style) in words {
        let w = word.width();
        if used > 0 && used + 1 + w > room {
            lines.push(vec![Span::raw("  ")]);
            used = 0;
        }
        let line = lines.last_mut().expect("starts with a line");
        if used > 0 {
            line.push(Span::raw(" "));
            used += 1;
        }
        line.push(Span::styled(word.to_owned(), style));
        used += w;
    }
    lines.into_iter().map(Line::from).collect()
}

/// The settings store; tests never touch the real one.
#[cfg(not(test))]
fn store() -> Option<Store> {
    Store::open(&crate::progress::default_path()).ok()
}

#[cfg(test)]
fn store() -> Option<Store> {
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crossterm::event::KeyModifiers;

    use super::*;

    const LOG: &str =
        "# Changelog\n\n## 1.1.0\n\n- Faster.\n- Run `leaf ui`.\n\n## 1.0.0\n\n- First.\n";

    fn memory() -> Store {
        Store::open(Path::new(":memory:")).expect("in-memory store opens")
    }

    #[test]
    fn this_version_has_notes() {
        assert!(
            !notes(CHANGELOG, VERSION).is_empty(),
            "add a `## {VERSION}` section to CHANGELOG.md"
        );
    }

    #[test]
    fn notes_are_the_lines_of_one_version() {
        assert_eq!(notes(LOG, "1.1.0"), ["Faster.", "Run `leaf ui`."]);
        assert_eq!(notes(LOG, "1.0.0"), ["First."]);
        assert!(notes(LOG, "2.0.0").is_empty());
    }

    #[test]
    fn shown_once_after_an_update() {
        let store = memory();
        store.save_seen_version("1.0.0").expect("saves");
        assert!(News::due(&store, "1.1.0", LOG).is_some());
        assert!(News::due(&store, "1.1.0", LOG).is_none(), "already seen");
    }

    #[test]
    fn a_fresh_install_shows_nothing_until_the_next_update() {
        let store = memory();
        assert!(News::due(&store, "1.0.0", LOG).is_none());
        assert!(News::due(&store, "1.1.0", LOG).is_some());
    }

    #[test]
    fn an_upgrade_from_before_the_notes_shows_them() {
        let store = memory();
        store.add_usage("2026-11-13", 60).expect("saves");
        assert!(News::due(&store, "1.1.0", LOG).is_some());
    }

    #[test]
    fn keys_wait_for_ok() {
        let press = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut news = Some(News {
            version: "1.1.0",
            notes: vec![],
            ok: Rect::default(),
        });
        assert!(News::key(&mut news, press(KeyCode::Char('j'))));
        assert!(news.is_some());
        assert!(News::key(&mut news, press(KeyCode::Enter)));
        assert!(news.is_none());
        assert!(
            !News::key(&mut news, press(KeyCode::Char('j'))),
            "gone: keys go to the view"
        );
    }

    #[test]
    fn long_notes_wrap_under_the_bullet() {
        let lines = bullet("one two three four", 11);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(text, ["• one two", "  three", "  four"]);
    }
}
