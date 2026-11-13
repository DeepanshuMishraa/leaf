//! The look of Rex's own pickers, for leaf's popovers in a floating Rex layer:
//! a chip naming the picker, a search field, then titled sections of rows,
//! the picked one filled with Rex's accent.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::render::DIM;

/// Rex's accent, as in its palette and pickers.
const ACCENT: Color = Color::Rgb(171, 74, 46);
const CHIP: Color = Color::Rgb(72, 72, 72);
const FIELD: Color = Color::Rgb(52, 52, 52);
const WHITE: Color = Color::Rgb(245, 245, 245);

pub struct Row {
    pub icon: &'static str,
    pub label: String,
    pub detail: String,
    pub checked: bool,
}

pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
}

/// What a key did to the search field and the picked row.
pub enum Edit {
    /// The text changed; the first row is picked again.
    Typed,
    Moved,
    /// Enter on the picked row.
    Choose,
    /// Tab: complete the picked row into the field.
    Complete,
    Quit,
    Nothing,
}

pub fn edit(
    code: KeyCode,
    mods: KeyModifiers,
    typed: &mut String,
    selected: &mut usize,
    rows: usize,
) -> Edit {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let last = rows.saturating_sub(1);
    match code {
        KeyCode::Esc => Edit::Quit,
        KeyCode::Char('c' | 'g') if ctrl => Edit::Quit,
        KeyCode::Enter => Edit::Choose,
        KeyCode::Tab => Edit::Complete,
        KeyCode::Up | KeyCode::BackTab => {
            *selected = selected.saturating_sub(1);
            Edit::Moved
        }
        KeyCode::Char('p' | 'k') if ctrl => {
            *selected = selected.saturating_sub(1);
            Edit::Moved
        }
        KeyCode::Down => {
            *selected = (*selected + 1).min(last);
            Edit::Moved
        }
        KeyCode::Char('n' | 'j') if ctrl => {
            *selected = (*selected + 1).min(last);
            Edit::Moved
        }
        KeyCode::Backspace => {
            typed.pop();
            *selected = 0;
            Edit::Typed
        }
        KeyCode::Char('u') if ctrl => {
            typed.clear();
            *selected = 0;
            Edit::Typed
        }
        KeyCode::Char(c) if !ctrl => {
            typed.push(c);
            *selected = 0;
            Edit::Typed
        }
        _ => Edit::Nothing,
    }
}

pub struct View<'a> {
    pub chip: &'a str,
    pub typed: &'a str,
    pub placeholder: &'a str,
    pub sections: &'a [Section],
    pub selected: usize,
    /// Shown when no section has a row.
    pub empty: &'a str,
    pub footer: &'a str,
}

pub fn draw(f: &mut Frame, view: &View) {
    let area = f.area();
    let width = area.width.saturating_sub(4);
    let x = area.x + 2;
    let mut y = area.y + 1;
    // The field: a chip, then the text with a cursor bar.
    let chip = format!(" {} ", view.chip);
    let chip_width = chip.width() as u16;
    let room = (width as usize).saturating_sub(chip_width as usize + 3);
    let shown = tail(view.typed, room.saturating_sub(1));
    let mut field = vec![
        Span::styled(" ", Style::default().bg(FIELD)),
        Span::styled(
            chip,
            Style::default()
                .bg(CHIP)
                .fg(WHITE)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", Style::default().bg(FIELD)),
    ];
    if view.typed.is_empty() {
        field.push(Span::styled("▏", Style::default().bg(FIELD).fg(ACCENT)));
        field.push(Span::styled(
            view.placeholder.to_owned(),
            Style::default().bg(FIELD).fg(DIM),
        ));
    } else {
        field.push(Span::styled(shown, Style::default().bg(FIELD).fg(WHITE)));
        field.push(Span::styled("▏", Style::default().bg(FIELD).fg(ACCENT)));
    }
    let used: usize = field.iter().map(|s| s.content.width()).sum();
    field.push(Span::styled(
        " ".repeat((width as usize).saturating_sub(used)),
        Style::default().bg(FIELD),
    ));
    f.render_widget(Paragraph::new(Line::from(field)), Rect::new(x, y, width, 1));
    y += 2;

    let bottom = area.y + area.height.saturating_sub(2);
    let rows_room = bottom.saturating_sub(y) as usize;
    let mut lines: Vec<(Line, Option<usize>)> = Vec::new();
    let mut index = 0;
    for section in view.sections.iter().filter(|s| !s.rows.is_empty()) {
        if !section.title.is_empty() {
            lines.push((
                Line::from(Span::styled(
                    format!(" {}", section.title),
                    Style::default().fg(DIM),
                )),
                None,
            ));
        }
        for row in &section.rows {
            lines.push((row_line(row, index == view.selected, width), Some(index)));
            index += 1;
        }
    }
    if lines.is_empty() {
        lines.push((
            Line::from(Span::styled(
                format!(" {}", view.empty),
                Style::default().fg(DIM),
            )),
            None,
        ));
    }
    // Scroll so the picked row stays in view.
    let at = lines
        .iter()
        .position(|(_, i)| *i == Some(view.selected))
        .unwrap_or(0);
    let first = at.saturating_sub(rows_room.saturating_sub(1));
    let visible: Vec<Line> = lines
        .into_iter()
        .skip(first)
        .take(rows_room)
        .map(|(line, _)| line)
        .collect();
    f.render_widget(
        Paragraph::new(visible),
        Rect::new(x, y, width, rows_room as u16),
    );
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            view.footer.to_owned(),
            Style::default().fg(DIM),
        ))),
        Rect::new(x + 1, bottom + 1, width.saturating_sub(1), 1),
    );
}

fn row_line(row: &Row, picked: bool, width: u16) -> Line<'static> {
    let (base, detail) = if picked {
        (
            Style::default().bg(ACCENT).fg(WHITE),
            Style::default().bg(ACCENT).fg(WHITE),
        )
    } else {
        (Style::default(), Style::default().fg(DIM))
    };
    let check = if row.checked { "✓ " } else { "  " };
    let left = format!(
        "  {}{}",
        if row.icon.is_empty() {
            String::new()
        } else {
            format!("{} ", row.icon)
        },
        row.label
    );
    let right = format!("{}{}", row.detail, check);
    let room = (width as usize).saturating_sub(left.width() + right.width() + 1);
    let left = if room == 0 {
        tail_front(&left, (width as usize).saturating_sub(right.width() + 2))
    } else {
        left
    };
    let gap = (width as usize).saturating_sub(left.width() + right.width());
    Line::from(vec![
        Span::styled(
            left,
            base.add_modifier(if picked {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::styled(" ".repeat(gap), base),
        Span::styled(right, detail),
    ])
}

/// The end of `text` that fits in `room` columns.
fn tail(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_owned();
    }
    let mut out: Vec<char> = Vec::new();
    let mut used = 1;
    for c in text.chars().rev() {
        used += c.to_string().width();
        if used > room {
            break;
        }
        out.push(c);
    }
    out.reverse();
    format!("…{}", out.into_iter().collect::<String>())
}

/// The start of `text` that fits in `room` columns.
fn tail_front(text: &str, room: usize) -> String {
    let mut out = String::new();
    let mut used = 1;
    for c in text.chars() {
        used += c.to_string().width();
        if used > room {
            break;
        }
        out.push(c);
    }
    format!("{out}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_paths_keep_their_end() {
        assert_eq!(tail("/Users/me/Code/leaf/", 10), "…ode/leaf/");
        assert_eq!(tail("short", 10), "short");
    }

    #[test]
    fn keys_move_within_the_rows() {
        let (mut typed, mut selected) = (String::new(), 0);
        edit(
            KeyCode::Down,
            KeyModifiers::NONE,
            &mut typed,
            &mut selected,
            3,
        );
        edit(
            KeyCode::Down,
            KeyModifiers::NONE,
            &mut typed,
            &mut selected,
            3,
        );
        edit(
            KeyCode::Down,
            KeyModifiers::NONE,
            &mut typed,
            &mut selected,
            3,
        );
        assert_eq!(selected, 2);
        edit(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            &mut typed,
            &mut selected,
            3,
        );
        assert_eq!((typed.as_str(), selected), ("a", 0));
    }
}
