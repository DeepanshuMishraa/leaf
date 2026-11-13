//! The home screen `leaf` opens on when nothing is named: a greeting, the
//! date and how long leaf was used today, then a search over recent
//! documents and the folder it started in.
//!
//! Kept quiet on purpose: plain text on one left edge, bold for what's
//! picked or typed, dim for everything around it, and nothing else. The
//! column sits at a fixed height so it doesn't jump as the list narrows.

use std::collections::HashMap;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::picker::{self, Entry};
use crate::render::DIM;
use crate::usage::{self, Date};

/// Widest the home column gets.
const WIDTH: u16 = 60;
/// Columns before the text edge, where the picked row's mark sits.
const GUTTER: u16 = 2;
/// Most rows the list takes, so a big folder doesn't fill the screen.
const LIST_ROWS: u16 = 16;

pub struct View<'a> {
    pub name: &'a str,
    /// Seconds used, by day key.
    pub usage: &'a HashMap<String, u64>,
    pub typed: &'a str,
    /// Where the picker started, so recent paths read short.
    pub here: &'a Path,
    pub entries: &'a [(String, Vec<Entry>)],
    pub selected: usize,
}

pub fn draw(f: &mut Frame, view: &View) {
    let area = f.area();
    if area.width < 24 || area.height < 10 {
        return;
    }
    let width = area.width.saturating_sub(4).min(WIDTH);
    let x = area.x + (area.width - width) / 2;
    // Everything's text starts here; the gutter left of it holds the mark.
    let text = Rect::new(x + GUTTER, 0, width - GUTTER, 1);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let footer_y = area.bottom() - 1;

    // A quarter down, a little above the optical middle, whatever is listed.
    let mut y = area.y + (area.height.saturating_sub(2) / 4).clamp(1, 8);
    let today = Date::today();

    let greeting = match view.name {
        "" => greeting(usage::hour()).to_owned(),
        name => format!("{}, {name}", greeting(usage::hour())),
    };
    f.render_widget(
        Paragraph::new(Span::styled(greeting, bold)),
        Rect { y, ..text },
    );
    y += 1;
    let mut summary = format!(
        "{} {} {}",
        today.weekday_name(),
        today.day,
        today.month_name()
    );
    let spent = view.usage.get(&today.key()).copied().unwrap_or(0);
    if spent > 0 {
        summary.push_str(&format!(" · {} read today", usage::brief(spent)));
    }
    f.render_widget(
        Paragraph::new(Span::styled(summary, dim)),
        Rect { y, ..text },
    );
    y += 3;

    // The field: what's typed in bold after its folder, else a hint.
    let (folder, filter) = field_parts(view.typed, view.here);
    let spans = if filter.is_empty() && folder.is_empty() {
        vec![
            Span::styled("▏", bold),
            Span::styled("Search, or type a path or link", dim),
        ]
    } else {
        vec![
            Span::styled(folder, dim),
            Span::styled(filter, bold),
            Span::styled("▏", bold),
        ]
    };
    f.render_widget(Paragraph::new(Line::from(spans)), Rect { y, ..text });
    y += 2;

    let bottom = footer_y.saturating_sub(1);
    let rows = bottom.saturating_sub(y).min(LIST_ROWS);
    list(f, view, Rect::new(x, y, width, rows));

    // Keys plain, what they do dim.
    let mut keys = Vec::new();
    for (key, does) in [("↑↓", "move"), ("enter", "open"), ("esc", "quit")] {
        if !keys.is_empty() {
            keys.push(Span::raw("   "));
        }
        keys.push(Span::raw(key));
        keys.push(Span::styled(format!(" {does}"), dim));
    }
    f.render_widget(
        Paragraph::new(Line::from(keys)).centered(),
        Rect::new(area.x, footer_y, area.width, 1),
    );
}

/// Good morning, afternoon, evening or night, by the local hour.
fn greeting(hour: u8) -> &'static str {
    match hour {
        5..=11 => "Good morning",
        12..=16 => "Good afternoon",
        17..=21 => "Good evening",
        _ => "Good night",
    }
}

/// The field as shown: the folder it's in, shortened, and what's typed after it.
fn field_parts(typed: &str, here: &Path) -> (String, String) {
    let start = format!("{}/", here.display().to_string().trim_end_matches('/'));
    if let Some(filter) = typed.strip_prefix(&start)
        && !filter.contains('/')
    {
        return (String::new(), filter.to_owned());
    }
    match typed.rfind('/') {
        Some(at) => (picker::tilde(&typed[..=at]), typed[at + 1..].to_owned()),
        None => (String::new(), typed.to_owned()),
    }
}

fn list(f: &mut Frame, view: &View, area: Rect) {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if area.height == 0 {
        return;
    }
    let text_x = area.x + GUTTER;
    let text_width = area.width - GUTTER;
    // Every row: a section title, or an entry and its index among entries.
    let mut rows: Vec<Option<(usize, &Entry)>> = Vec::new();
    let mut titles: HashMap<usize, &str> = HashMap::new();
    let mut index = 0;
    for (title, entries) in view.entries.iter().filter(|(_, e)| !e.is_empty()) {
        if !rows.is_empty() {
            rows.push(None);
        }
        if !title.is_empty() {
            titles.insert(rows.len(), title);
            rows.push(None);
        }
        for entry in entries {
            rows.push(Some((index, entry)));
            index += 1;
        }
    }
    if index == 0 {
        f.buffer_mut().set_string(text_x, area.y, "No matches", dim);
        return;
    }
    let picked_row = rows
        .iter()
        .position(|row| row.is_some_and(|(i, _)| i == view.selected))
        .unwrap_or(0);
    let room = area.height as usize;
    let first = picked_row.saturating_sub(room.saturating_sub(1));
    for (offset, row) in rows.iter().enumerate().skip(first).take(room) {
        let y = area.y + (offset - first) as u16;
        if let Some(title) = titles.get(&offset) {
            f.buffer_mut().set_string(text_x, y, *title, dim);
            continue;
        }
        let Some((i, entry)) = row else {
            continue;
        };
        let picked = *i == view.selected;
        let (name, place, percent) = describe(entry, view.here);
        // How far shows only on a document part read.
        let percent = percent.filter(|p| (1..100).contains(p));
        let right = if percent.is_some() { 6 } else { 0 };
        let room = (text_width as usize).saturating_sub(right);
        let name = clip(&name, room);
        let place = clip(&place, room.saturating_sub(name.width() + 2));
        if picked {
            f.buffer_mut().set_string(area.x, y, "›", bold);
        }
        let mut spans = vec![Span::styled(
            name,
            if picked { bold } else { Style::default() },
        )];
        if !place.is_empty() {
            spans.push(Span::styled(format!("  {place}"), dim));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(text_x, y, text_width, 1),
        );
        if let Some(percent) = percent {
            f.buffer_mut()
                .set_string(area.right() - 4, y, format!("{percent:>3}%"), dim);
        }
    }
}

/// An entry as the list shows it: its name, where it is, and how far it
/// was read. Folders end in a slash; links lose their scheme.
fn describe(entry: &Entry, here: &Path) -> (String, String, Option<usize>) {
    let split = |path: &Path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // Nothing when it's right here, relative below here, else in full.
        let parent = path
            .parent()
            .map(|p| match p.strip_prefix(here) {
                Ok(below) => below.display().to_string(),
                Err(_) => picker::tilde(&p.display().to_string()),
            })
            .unwrap_or_default();
        (name, parent)
    };
    match entry {
        Entry::Folder(path) => (format!("{}/", split(path).0), String::new(), None),
        Entry::Document(path) => (split(path).0, String::new(), None),
        Entry::Recent { target, percent } if picker::is_link(target) => {
            (bare(target).to_owned(), String::new(), Some(*percent))
        }
        Entry::Recent { target, percent } => {
            let (name, parent) = split(Path::new(target));
            (name, parent, Some(*percent))
        }
        Entry::Link(url) => (format!("Open {}", bare(url)), String::new(), None),
    }
}

/// A link without its scheme.
fn bare(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, rest)| rest)
}

/// `text` cut to `width` columns, with an ellipsis when cut.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + 2 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// The first name of the account leaf runs as, from its full name in the
/// system's user records, else the login name; empty when neither says.
pub fn first_name() -> String {
    // SAFETY: getpwuid returns a pointer into a static record, read at once
    // and copied out before anything else could call it.
    let full = unsafe {
        let record = libc::getpwuid(libc::getuid());
        if record.is_null() || (*record).pw_gecos.is_null() {
            None
        } else {
            Some(
                std::ffi::CStr::from_ptr((*record).pw_gecos)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    let first = |text: &str| {
        text.split(',')
            .next()
            .and_then(|name| name.split_whitespace().next())
            .map(str::to_owned)
    };
    full.as_deref()
        .and_then(first)
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|u| !u.is_empty())
                .map(|u| {
                    let mut chars = u.chars();
                    chars
                        .next()
                        .map(|c| c.to_uppercase().chain(chars).collect())
                        .unwrap_or_default()
                })
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_greeting_follows_the_hour() {
        assert_eq!(greeting(7), "Good morning");
        assert_eq!(greeting(13), "Good afternoon");
        assert_eq!(greeting(19), "Good evening");
        assert_eq!(greeting(2), "Good night");
    }

    #[test]
    fn recent_documents_say_where_they_are_only_when_elsewhere() {
        let here = Path::new("/home/a/docs");
        let recent = |target: &str| Entry::Recent {
            target: target.to_owned(),
            percent: 10,
        };
        assert_eq!(describe(&recent("/home/a/docs/x.md"), here).1, "");
        assert_eq!(describe(&recent("/home/a/docs/sub/x.md"), here).1, "sub");
        assert_eq!(
            describe(&recent("https://example.com/essay"), here).0,
            "example.com/essay"
        );
    }

    #[test]
    fn the_field_shows_the_filter_alone_where_it_started() {
        let here = Path::new("/home/a/docs");
        assert_eq!(
            field_parts("/home/a/docs/pap", here),
            (String::new(), "pap".to_owned())
        );
        assert_eq!(
            field_parts("/elsewhere/books/x", here),
            ("/elsewhere/books/".to_owned(), "x".to_owned())
        );
    }
}
