//! A spreadsheet or CSV file as a grid of cells you move around, the way a
//! spreadsheet app shows it: column letters and row numbers, the first row
//! pinned as the header, a cursor cell and a selection you make with keys or
//! the mouse. Sorting and filtering change only what is shown; the file is
//! never written. Cells come from `sheet::load`, so CSV, TSV, Excel and
//! OpenDocument files all open here; `leaf open` sends them here instead of
//! to the reader.

use std::cmp::Ordering;
use std::io::{self, Write as _};
use std::path::Path;

use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::grid::letters;
use crate::keymap::{Action, Keymap};
use crate::render::DIM;
use crate::sheet::{self, Sheet, SheetFailed};
use crate::sheetkeys::SheetAction;
use crate::theme::{self, Theme};

/// Narrowest and widest a column opens at, in cells, before padding.
const MIN_WIDTH: u16 = 3;
const MAX_WIDTH: u16 = 40;
/// Rows looked at to size a column, so a long file opens at once.
const SIZING_ROWS: usize = 2000;
/// Rows the wheel scrolls at a time.
const WHEEL: usize = 3;
/// What a spreadsheet shows in a cell whose formula failed; drawn in red.
const ERRORS: [&str; 10] = [
    "#DIV/0!",
    "#N/A",
    "#NAME?",
    "#NULL!",
    "#NUM!",
    "#REF!",
    "#VALUE!",
    "#SPILL!",
    "#CALC!",
    "#GETTING_DATA",
];

#[derive(Debug, thiserror::Error)]
pub enum SheetViewFailed {
    #[error("{0}")]
    Load(#[from] SheetFailed),
    #[error("the terminal failed ({0}). The file was not changed.")]
    Terminal(#[from] io::Error),
}

/// A cell, by its line on screen (0 is the header row, then the shown rows
/// in order) and its column in the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pos {
    row: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    Up,
    Down,
}

/// One sheet and how it is being looked at.
struct Tab {
    sheet: Sheet,
    widths: Vec<u16>,
    /// Whether most of a column's cells are numbers, so its header lines up
    /// with them on the right.
    numeric: Vec<bool>,
    /// The rows shown under the header, by index into `sheet.rows`, in order.
    order: Vec<usize>,
    sort: Option<(usize, Sort)>,
    /// The column and the text its cells must hold to be shown.
    filter: Option<(usize, String)>,
    cursor: Pos,
    /// The other corner of the selection, while there is one.
    anchor: Option<Pos>,
    /// The first shown row under the header (an index into `order`) and the
    /// first shown column.
    top: usize,
    left: usize,
    /// Whether the next draw scrolls to the cursor. Off after the wheel.
    follow: bool,
}

/// A cell's text as a number, for sorting, totals and alignment. Thousands
/// separators, a currency sign and a percent sign are allowed.
fn value(text: &str) -> Option<f64> {
    let t = text.trim();
    let (negative, t) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t),
    };
    let t = t.strip_prefix(['$', '€', '£', '¥']).unwrap_or(t);
    let (t, percent) = match t.strip_suffix('%') {
        Some(rest) => (rest, true),
        None => (t, false),
    };
    if !t.starts_with(|c: char| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let plain: String = if grouped(t) {
        t.chars().filter(|&c| c != ',').collect()
    } else {
        t.to_owned()
    };
    let n: f64 = plain.parse().ok().filter(|n: &f64| n.is_finite())?;
    let n = if percent { n / 100.0 } else { n };
    Some(if negative { -n } else { n })
}

/// Whether `t` groups its whole part by thousands with commas, as `1,234.5`.
fn grouped(t: &str) -> bool {
    let whole = t.split('.').next().unwrap_or(t);
    let groups: Vec<&str> = whole.split(',').collect();
    groups.len() > 1
        && (1..=3).contains(&groups[0].len())
        && groups[1..].iter().all(|g| g.len() == 3)
        && groups.iter().all(|g| g.chars().all(|c| c.is_ascii_digit()))
}

/// A number as the status line shows it: grouped by thousands.
fn shown_number(n: f64) -> String {
    let text = sheet::number(n);
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text.as_str()),
    };
    let (whole, fraction) = match rest.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (rest, None),
    };
    match (whole.parse::<usize>(), fraction) {
        (Ok(w), Some(f)) => format!("{sign}{}.{f}", sheet::thousands(w)),
        (Ok(w), None) => format!("{sign}{}", sheet::thousands(w)),
        _ => text,
    }
}

/// A column's width from its widest cell among the first rows.
fn widths(sheet: &Sheet) -> Vec<u16> {
    let columns = sheet.rows.first().map_or(0, Vec::len);
    (0..columns)
        .map(|c| {
            sheet
                .rows
                .iter()
                .take(SIZING_ROWS)
                .filter_map(|row| row.get(c))
                .map(|cell| one_line(cell).width())
                .max()
                .unwrap_or(0)
                .clamp(MIN_WIDTH as usize, MAX_WIDTH as usize) as u16
        })
        .collect()
}

/// Whether more than half of each column's filled cells under the header
/// are numbers, among the first rows.
fn numeric(sheet: &Sheet) -> Vec<bool> {
    let columns = sheet.rows.first().map_or(0, Vec::len);
    (0..columns)
        .map(|c| {
            let (numbers, filled) = sheet
                .rows
                .iter()
                .skip(1)
                .take(SIZING_ROWS)
                .filter_map(|row| row.get(c))
                .filter(|cell| !cell.trim().is_empty())
                .fold((0, 0), |(n, f), cell| {
                    (n + usize::from(value(cell).is_some()), f + 1)
                });
            numbers * 2 > filled
        })
        .collect()
}

/// Cell text on one line: line breaks and tabs become spaces.
fn one_line(text: &str) -> String {
    text.trim()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}

/// `text` cut to `width` cells, with `…` when something was cut.
fn fit(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn compare(a: &str, b: &str) -> Ordering {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    match (value(a), value(b)) {
        (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.to_lowercase().cmp(&b.to_lowercase()),
    }
}

impl Tab {
    fn new(sheet: Sheet) -> Tab {
        let order = (1..sheet.rows.len()).collect();
        Tab {
            widths: widths(&sheet),
            numeric: numeric(&sheet),
            sheet,
            order,
            sort: None,
            filter: None,
            cursor: Pos { row: 0, col: 0 },
            anchor: None,
            top: 0,
            left: 0,
            follow: true,
        }
    }

    fn columns(&self) -> usize {
        self.widths.len()
    }

    /// Lines on screen: the header and the shown rows. 0 for an empty sheet.
    fn lines(&self) -> usize {
        if self.sheet.rows.is_empty() {
            0
        } else {
            self.order.len() + 1
        }
    }

    /// The row in `sheet.rows` a line shows.
    fn source(&self, line: usize) -> Option<usize> {
        match line {
            0 => (!self.sheet.rows.is_empty()).then_some(0),
            n => self.order.get(n - 1).copied(),
        }
    }

    fn cell(&self, pos: Pos) -> &str {
        self.source(pos.row)
            .and_then(|r| self.sheet.rows.get(r))
            .and_then(|row| row.get(pos.col))
            .map_or("", String::as_str)
    }

    /// Where a cell sits in the file, as `B12`.
    fn address(&self, pos: Pos) -> String {
        let (top, left) = self.sheet.origin;
        let row = self.source(pos.row).unwrap_or(0);
        format!("{}{}", letters(left + pos.col), top + row + 1)
    }

    fn formula(&self, pos: Pos) -> Option<&str> {
        let (top, left) = self.sheet.origin;
        let row = self.source(pos.row)?;
        self.sheet
            .formulas
            .get(&(top + row, left + pos.col))
            .map(String::as_str)
    }

    /// Moves the cursor, growing the selection when `extend`.
    fn go(&mut self, to: Pos, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = Pos {
            row: to.row.min(self.lines().saturating_sub(1)),
            col: to.col.min(self.columns().saturating_sub(1)),
        };
        self.follow = true;
    }

    fn step(&mut self, rows: isize, cols: isize, extend: bool) {
        let to = Pos {
            row: self.cursor.row.saturating_add_signed(rows),
            col: self.cursor.col.saturating_add_signed(cols),
        };
        self.go(to, extend);
    }

    /// The selection's corners: top left and bottom right.
    fn selection(&self) -> (Pos, Pos) {
        let a = self.anchor.unwrap_or(self.cursor);
        let b = self.cursor;
        (
            Pos {
                row: a.row.min(b.row),
                col: a.col.min(b.col),
            },
            Pos {
                row: a.row.max(b.row),
                col: a.col.max(b.col),
            },
        )
    }

    fn selected(&self, pos: Pos) -> bool {
        let (from, to) = self.selection();
        (from.row..=to.row).contains(&pos.row) && (from.col..=to.col).contains(&pos.col)
    }

    /// The selection as tab-separated text, one line per row.
    fn copy_text(&self) -> String {
        let (from, to) = self.selection();
        (from.row..=to.row)
            .map(|row| {
                (from.col..=to.col)
                    .map(|col| one_line(self.cell(Pos { row, col })))
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many selected cells hold something, and the sum of those that
    /// hold numbers with how many there are.
    fn stats(&self) -> (usize, f64, usize) {
        let (from, to) = self.selection();
        let mut filled = 0;
        let mut sum = 0.0;
        let mut numbers = 0;
        for row in from.row..=to.row {
            for col in from.col..=to.col {
                let text = self.cell(Pos { row, col });
                if text.trim().is_empty() {
                    continue;
                }
                filled += 1;
                if row > 0
                    && let Some(n) = value(text)
                {
                    sum += n;
                    numbers += 1;
                }
            }
        }
        (filled, sum, numbers)
    }

    /// Sorts and filters the rows again, keeping the cursor on its row when
    /// that row is still shown.
    fn arrange(&mut self) {
        let keep = self.source(self.cursor.row);
        let rows = &self.sheet.rows;
        let mut order: Vec<usize> = (1..rows.len())
            .filter(|&r| match &self.filter {
                Some((col, text)) => rows[r]
                    .get(*col)
                    .is_some_and(|cell| cell.to_lowercase().contains(&text.to_lowercase())),
                None => true,
            })
            .collect();
        if let Some((col, sort)) = self.sort {
            let key = |r: &usize| rows[*r].get(col).map_or("", String::as_str);
            order.sort_by(|a, b| {
                let ordering = compare(key(a), key(b));
                match sort {
                    // Empty cells stay at the bottom either way.
                    Sort::Down if !key(a).trim().is_empty() && !key(b).trim().is_empty() => {
                        ordering.reverse()
                    }
                    _ => ordering,
                }
            });
        }
        self.order = order;
        self.anchor = None;
        let row = match keep {
            Some(0) | None => 0,
            Some(r) => self.order.iter().position(|&o| o == r).map_or(0, |i| i + 1),
        };
        self.go(
            Pos {
                row,
                col: self.cursor.col,
            },
            false,
        );
    }

    /// Up, down, then as the file has it.
    fn cycle_sort(&mut self) {
        let col = self.cursor.col;
        self.sort = match self.sort {
            Some((c, Sort::Up)) if c == col => Some((col, Sort::Down)),
            Some((c, Sort::Down)) if c == col => None,
            _ => Some((col, Sort::Up)),
        };
        self.arrange();
    }

    /// The next cell after (or before) the cursor that holds `query`,
    /// reading row by row and wrapping around.
    fn find(&self, query: &str, forward: bool) -> Option<Pos> {
        let query = query.to_lowercase();
        let columns = self.columns();
        let total = self.lines() * columns;
        if total == 0 || query.is_empty() {
            return None;
        }
        let at = self.cursor.row * columns + self.cursor.col;
        (1..=total)
            .map(|step| {
                if forward {
                    (at + step) % total
                } else {
                    (at + total - step % total) % total
                }
            })
            .map(|i| Pos {
                row: i / columns,
                col: i % columns,
            })
            .find(|&pos| self.cell(pos).to_lowercase().contains(&query))
    }

    /// The cell `text` names, such as `B12`, `b12` or `12`, in the file's
    /// own letters and numbers.
    fn locate(&self, text: &str) -> Result<Pos, String> {
        let text = text.trim().to_ascii_uppercase();
        let split = text
            .find(|c: char| c.is_ascii_digit())
            .unwrap_or(text.len());
        let (col_text, row_text) = text.split_at(split);
        let (top, left) = self.sheet.origin;
        let col = if col_text.is_empty() {
            self.cursor.col
        } else if col_text.chars().all(|c| c.is_ascii_uppercase()) {
            let number = col_text
                .bytes()
                .fold(0usize, |n, b| n * 26 + usize::from(b - b'A') + 1)
                - 1;
            number
                .checked_sub(left)
                .filter(|&c| c < self.columns())
                .ok_or_else(|| format!("column {col_text} is outside this sheet's cells"))?
        } else {
            return Err(format!(
                "`{text}` is not a cell. Name one such as B12, or a row number"
            ));
        };
        let row = if row_text.is_empty() {
            self.cursor.row
        } else {
            let number: usize = row_text.parse().map_err(|_| {
                format!("`{text}` is not a cell. Name one such as B12, or a row number")
            })?;
            let source = number
                .checked_sub(top + 1)
                .filter(|&r| r < self.sheet.rows.len())
                .ok_or_else(|| format!("row {number} is outside this sheet's cells"))?;
            match source {
                0 => 0,
                r => self
                    .order
                    .iter()
                    .position(|&o| o == r)
                    .map(|i| i + 1)
                    .ok_or_else(|| {
                        format!("row {number} is hidden by the filter. Press esc to show every row")
                    })?,
            }
        };
        Ok(Pos { row, col })
    }

    fn resize(&mut self, by: i16) {
        if let Some(w) = self.widths.get_mut(self.cursor.col) {
            *w = w.saturating_add_signed(by).clamp(MIN_WIDTH, 200);
        }
    }
}

/// Text being typed at the bottom of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Prompt {
    Find(String),
    GoTo(String),
    Filter(String),
}

impl Prompt {
    fn text(&mut self) -> &mut String {
        match self {
            Prompt::Find(t) | Prompt::GoTo(t) | Prompt::Filter(t) => t,
        }
    }
}

/// Where things were drawn, for the mouse.
#[derive(Debug, Default, Clone)]
struct Geometry {
    gutter: u16,
    letters_y: u16,
    header_y: u16,
    body_y: u16,
    body_rows: u16,
    /// Each shown column's first and last x, and its index.
    columns: Vec<(u16, u16, usize)>,
    status_y: u16,
    /// Each sheet tab's first and last x, and its index.
    tabs: Vec<(u16, u16, usize)>,
}

#[derive(Debug, Clone, Copy)]
struct Colors {
    cursor: Style,
    selection: Style,
    accent: Style,
}

impl Colors {
    fn of(theme: Option<Theme>) -> Colors {
        match theme {
            Some(t) => {
                let rgb = |[r, g, b]: [u8; 3]| Color::Rgb(r, g, b);
                Colors {
                    cursor: Style::default().bg(rgb(t.accent)).fg(rgb(t.background)),
                    selection: Style::default().bg(rgb(theme::mix(t.background, t.accent, 0.16))),
                    accent: Style::default()
                        .fg(rgb(t.accent))
                        .add_modifier(Modifier::BOLD),
                }
            }
            None => Colors {
                cursor: Style::default().add_modifier(Modifier::REVERSED),
                selection: Style::default().add_modifier(Modifier::UNDERLINED),
                accent: Style::default()
                    .fg(Color::Indexed(4))
                    .add_modifier(Modifier::BOLD),
            },
        }
    }
}

struct App {
    title: String,
    tabs: Vec<Tab>,
    current: usize,
    keys: Keymap<SheetAction>,
    colors: Colors,
    prompt: Option<Prompt>,
    /// The last text found, for `n` and `N`.
    found: Option<String>,
    notice: Option<String>,
    help: bool,
    copy: Option<String>,
    quit: bool,
    geometry: Geometry,
    /// Whether the left button is held after pressing on a cell.
    dragging: bool,
}

impl App {
    fn new(title: String, sheets: Vec<Sheet>, keys: Keymap<SheetAction>, colors: Colors) -> App {
        App {
            title,
            tabs: sheets.into_iter().map(Tab::new).collect(),
            current: 0,
            keys,
            colors,
            prompt: None,
            found: None,
            notice: None,
            help: false,
            copy: None,
            quit: false,
            geometry: Geometry::default(),
            dragging: false,
        }
    }

    fn tab(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.current)
    }

    fn switch(&mut self, to: usize) {
        if to < self.tabs.len() {
            self.current = to;
        }
    }

    fn body_rows(&self) -> usize {
        usize::from(self.geometry.body_rows.max(1))
    }

    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.notice = None;
        if self.help {
            self.help = false;
            return;
        }
        if let Some(prompt) = self.prompt.as_mut() {
            match key.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Enter => {
                    if let Some(prompt) = self.prompt.take() {
                        self.submit(prompt);
                    }
                }
                KeyCode::Backspace => {
                    prompt.text().pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    prompt.text().push(c);
                }
                _ => {}
            }
            return;
        }
        for action in self.keys.actions(&key) {
            if self.act(action) {
                return;
            }
        }
    }

    /// Does `action`; false when it doesn't apply, so a shared key can try the next.
    fn act(&mut self, action: SheetAction) -> bool {
        use SheetAction as A;
        let page = self.body_rows();
        match action {
            A::Quit => self.quit = true,
            A::Help => self.help = true,
            A::NextSheet => self.switch((self.current + 1) % self.tabs.len().max(1)),
            A::PreviousSheet => {
                let n = self.tabs.len().max(1);
                self.switch((self.current + n - 1) % n);
            }
            A::Find => self.prompt = Some(Prompt::Find(String::new())),
            A::GoTo => self.prompt = Some(Prompt::GoTo(String::new())),
            A::Filter => self.prompt = Some(Prompt::Filter(String::new())),
            A::NextMatch | A::PreviousMatch => {
                let Some(query) = self.found.clone() else {
                    self.notice = Some(format!(
                        "Nothing to find yet. Press {} to find text.",
                        self.keys.first(A::Find)
                    ));
                    return true;
                };
                self.find(&query, action == A::NextMatch);
            }
            A::Copy => {
                if let Some(tab) = self.tabs.get(self.current) {
                    let (from, to) = tab.selection();
                    let cells = (to.row - from.row + 1) * (to.col - from.col + 1);
                    self.copy = Some(tab.copy_text());
                    self.notice = Some(match cells {
                        1 => "Copied the cell.".to_owned(),
                        n => format!("Copied {} cells.", sheet::thousands(n)),
                    });
                }
            }
            action => {
                let Some(tab) = self.tab() else { return true };
                match action {
                    A::Escape => {
                        if tab.anchor.is_some() {
                            tab.anchor = None;
                        } else if tab.filter.is_some() {
                            tab.filter = None;
                            tab.arrange();
                        } else {
                            return false;
                        }
                    }
                    A::Left => tab.step(0, -1, false),
                    A::Right => tab.step(0, 1, false),
                    A::Up => tab.step(-1, 0, false),
                    A::Down => tab.step(1, 0, false),
                    A::SelectLeft => tab.step(0, -1, true),
                    A::SelectRight => tab.step(0, 1, true),
                    A::SelectUp => tab.step(-1, 0, true),
                    A::SelectDown => tab.step(1, 0, true),
                    A::RowStart => tab.go(
                        Pos {
                            col: 0,
                            ..tab.cursor
                        },
                        false,
                    ),
                    A::RowEnd => tab.go(
                        Pos {
                            col: usize::MAX,
                            ..tab.cursor
                        },
                        false,
                    ),
                    A::Top => tab.go(
                        Pos {
                            row: 0,
                            ..tab.cursor
                        },
                        false,
                    ),
                    A::Bottom => tab.go(
                        Pos {
                            row: usize::MAX,
                            ..tab.cursor
                        },
                        false,
                    ),
                    A::HalfScreenDown => tab.step((page / 2).max(1) as isize, 0, false),
                    A::HalfScreenUp => tab.step(-((page / 2).max(1) as isize), 0, false),
                    A::ScreenDown => tab.step(page as isize, 0, false),
                    A::ScreenUp => tab.step(-(page as isize), 0, false),
                    A::Sort => tab.cycle_sort(),
                    A::Wider => tab.resize(2),
                    A::Narrower => tab.resize(-2),
                    _ => return false,
                }
            }
        }
        true
    }

    fn find(&mut self, query: &str, forward: bool) {
        self.found = Some(query.to_owned());
        let Some(tab) = self.tabs.get_mut(self.current) else {
            return;
        };
        match tab.find(query, forward) {
            Some(pos) => tab.go(pos, false),
            None => self.notice = Some(format!("No cell holds \"{query}\".")),
        }
    }

    fn submit(&mut self, prompt: Prompt) {
        match prompt {
            Prompt::Find(text) if !text.is_empty() => self.find(&text, true),
            Prompt::Find(_) => {}
            Prompt::GoTo(text) => {
                let Some(tab) = self.tabs.get_mut(self.current) else {
                    return;
                };
                match tab.locate(&text) {
                    Ok(pos) => tab.go(pos, false),
                    Err(why) => self.notice = Some(format!("Can't go there: {why}.")),
                }
            }
            Prompt::Filter(text) => {
                let Some(tab) = self.tabs.get_mut(self.current) else {
                    return;
                };
                tab.filter = (!text.is_empty()).then_some((tab.cursor.col, text));
                tab.arrange();
                if tab.order.is_empty() && tab.filter.is_some() {
                    self.notice = Some("No row matches. Press esc to show every row.".to_owned());
                }
            }
        }
    }

    /// The cell under a point, by its line on screen and column.
    fn hit(&self, x: u16, y: u16) -> Option<Pos> {
        let g = &self.geometry;
        let tab = self.tabs.get(self.current)?;
        let col = g
            .columns
            .iter()
            .find(|(from, to, _)| (*from..=*to).contains(&x))
            .map(|(_, _, c)| *c)?;
        let row = if y == g.header_y {
            0
        } else if y >= g.body_y && y < g.body_y + g.body_rows {
            tab.top + usize::from(y - g.body_y) + 1
        } else {
            return None;
        };
        (row < tab.lines()).then_some(Pos { row, col })
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        let (x, y) = (mouse.column, mouse.row);
        let shift = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if shift => {
                self.scroll_sideways(mouse.kind == MouseEventKind::ScrollDown)
            }
            MouseEventKind::ScrollRight => self.scroll_sideways(true),
            MouseEventKind::ScrollLeft => self.scroll_sideways(false),
            MouseEventKind::ScrollDown => {
                if let Some(tab) = self.tab() {
                    let last = tab.order.len().saturating_sub(1);
                    tab.top = (tab.top + WHEEL).min(last);
                    tab.follow = false;
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(tab) = self.tab() {
                    tab.top = tab.top.saturating_sub(WHEEL);
                    tab.follow = false;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.notice = None;
                self.prompt = None;
                if y == self.geometry.status_y {
                    let tab = self
                        .geometry
                        .tabs
                        .iter()
                        .find(|(from, to, _)| (*from..=*to).contains(&x))
                        .map(|(_, _, i)| *i);
                    if let Some(i) = tab {
                        self.switch(i);
                    }
                    return;
                }
                let gutter = self.geometry.gutter;
                let letters_y = self.geometry.letters_y;
                let column = self
                    .geometry
                    .columns
                    .iter()
                    .find(|(from, to, _)| (*from..=*to).contains(&x))
                    .map(|(_, _, c)| *c);
                if y == letters_y
                    && let Some(col) = column
                {
                    // A column letter selects the whole column.
                    if let Some(tab) = self.tab() {
                        tab.go(Pos { row: 0, col }, false);
                        tab.go(
                            Pos {
                                row: usize::MAX,
                                col,
                            },
                            true,
                        );
                    }
                    return;
                }
                if x < gutter
                    && let Some(row) = self.hit(gutter, y).map(|p| p.row)
                {
                    // A row number selects the whole row.
                    if let Some(tab) = self.tab() {
                        tab.go(Pos { row, col: 0 }, false);
                        tab.go(
                            Pos {
                                row,
                                col: usize::MAX,
                            },
                            true,
                        );
                    }
                    return;
                }
                if let Some(pos) = self.hit(x, y) {
                    if let Some(tab) = self.tab() {
                        tab.go(pos, shift);
                        tab.follow = false;
                    }
                    self.dragging = true;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                if let Some(pos) = self.hit(x, y)
                    && let Some(tab) = self.tab()
                {
                    tab.go(pos, true);
                    tab.follow = false;
                }
            }
            MouseEventKind::Up(_) => self.dragging = false,
            _ => {}
        }
    }

    fn scroll_sideways(&mut self, right: bool) {
        if let Some(tab) = self.tab() {
            tab.left = if right {
                (tab.left + 1).min(tab.columns().saturating_sub(1))
            } else {
                tab.left.saturating_sub(1)
            };
            tab.follow = false;
        }
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        self.geometry = Geometry::default();
        if area.height < 6 || area.width < 20 {
            buf.set_string(
                area.x,
                area.y,
                fit(
                    "Make the pane bigger to see the sheet.",
                    usize::from(area.width),
                ),
                Style::default(),
            );
            return;
        }
        let status_y = area.bottom() - 1;
        self.geometry.status_y = status_y;
        let colors = self.colors;
        let Some(tab) = self.tabs.get_mut(self.current) else {
            return;
        };
        if tab.lines() == 0 {
            buf.set_string(
                area.x + 2,
                area.y + 1,
                "This sheet has no cells with anything in them.",
                Style::default().fg(DIM),
            );
        } else {
            draw_grid(tab, &colors, area, buf, &mut self.geometry);
        }
        self.draw_status(area, buf);
        if self.help {
            self.draw_help(area, buf);
        }
    }

    fn draw_status(&mut self, area: Rect, buf: &mut Buffer) {
        let y = self.geometry.status_y;
        let width = usize::from(area.width);
        let dim = Style::default().fg(DIM);
        if let Some(prompt) = &self.prompt {
            let (label, text) = match prompt {
                Prompt::Find(t) => ("Find".to_owned(), t),
                Prompt::GoTo(t) => ("Go to cell".to_owned(), t),
                Prompt::Filter(t) => {
                    let col = self
                        .tabs
                        .get(self.current)
                        .map_or(0, |tab| tab.sheet.origin.1 + tab.cursor.col);
                    (format!("Show rows whose {} holds", letters(col)), t)
                }
            };
            let line = format!(" {label}: {text}▏");
            buf.set_string(area.x, y, fit(&line, width), self.colors.accent);
            let hint = "enter · esc cancel ";
            if line.width() + hint.width() + 2 < width {
                buf.set_string(area.right() - hint.width() as u16, y, hint, dim);
            }
            return;
        }
        // Sheet tabs, or the file's name for a single sheet.
        let mut x = area.x + 1;
        if self.tabs.len() > 1 {
            for (i, tab) in self.tabs.iter().enumerate() {
                let label = format!(" {} ", tab.sheet.name);
                let w = label.width() as u16;
                if x + w >= area.right() {
                    break;
                }
                let style = if i == self.current {
                    self.colors.cursor
                } else {
                    dim
                };
                buf.set_string(x, y, &label, style);
                self.geometry.tabs.push((x, x + w - 1, i));
                x += w + 1;
            }
        } else {
            let title = fit(&self.title, width / 2);
            buf.set_string(x, y, &title, Style::default().add_modifier(Modifier::BOLD));
            x += title.width() as u16 + 1;
        }
        let right = match &self.notice {
            Some(notice) => notice.clone(),
            None => self.summary(),
        };
        let room = usize::from(area.right().saturating_sub(x + 1));
        let right = fit(&right, room);
        let style = if self.notice.is_some() {
            self.colors.accent
        } else {
            dim
        };
        buf.set_string(area.right() - 1 - right.width() as u16, y, &right, style);
    }

    /// What the status line says on the right: totals for a selection, else
    /// where the cursor is, with any sort or filter.
    fn summary(&self) -> String {
        let Some(tab) = self.tabs.get(self.current) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if tab.anchor.is_some() {
            let (filled, sum, numbers) = tab.stats();
            parts.push(format!("Count {}", sheet::thousands(filled)));
            if numbers > 0 {
                parts.push(format!("Sum {}", shown_number(sum)));
                parts.push(format!(
                    "Average {}",
                    shown_number((sum / numbers as f64 * 100.0).round() / 100.0)
                ));
            }
        } else {
            let rows = tab.order.len();
            let all = tab.sheet.rows.len().saturating_sub(1);
            parts.push(if rows == all {
                format!("{} rows", sheet::thousands(rows))
            } else {
                format!(
                    "{} of {} rows",
                    sheet::thousands(rows),
                    sheet::thousands(all)
                )
            });
        }
        if let Some((col, sort)) = tab.sort {
            let arrow = if sort == Sort::Up { "↑" } else { "↓" };
            parts.push(format!(
                "sorted by {} {arrow}",
                letters(tab.sheet.origin.1 + col)
            ));
        }
        if let Some((col, text)) = &tab.filter {
            parts.push(format!(
                "{} holds \"{text}\"",
                letters(tab.sheet.origin.1 + col)
            ));
        }
        parts.push(format!("{} keys", self.keys.first(SheetAction::Help)));
        parts.join(" · ")
    }

    fn draw_help(&self, area: Rect, buf: &mut Buffer) {
        let lines: Vec<(String, &str)> = SheetAction::TABLE
            .iter()
            .zip(SheetAction::ABOUT)
            .map(|((_, action, _), about)| (self.keys.label(*action), *about))
            .collect();
        let key_width = lines.iter().map(|(k, _)| k.width()).max().unwrap_or(0);
        let width = (area.width.saturating_sub(4)).min(76);
        let height = (lines.len() as u16 + 4).min(area.height.saturating_sub(2));
        let x = area.x + (area.width - width) / 2;
        let y = area.y + (area.height - height) / 2;
        let popup = Rect::new(x, y, width, height);
        for row in popup.top()..popup.bottom() {
            for col in popup.left()..popup.right() {
                if let Some(cell) = buf.cell_mut((col, row)) {
                    cell.reset();
                }
            }
        }
        buf.set_string(
            x + 2,
            y + 1,
            "Keys",
            Style::default().add_modifier(Modifier::BOLD),
        );
        let room = usize::from(width.saturating_sub(4));
        for (i, (keys, about)) in lines.iter().enumerate() {
            let row = y + 3 + i as u16;
            if row >= popup.bottom() {
                break;
            }
            let line = format!("{keys:<key_width$}  {about}");
            buf.set_string(x + 2, row, fit(&line, room), Style::default());
            buf.set_string(x + 2, row, fit(keys, room), self.colors.accent);
        }
    }
}

/// The grid: the cell bar, column letters, the pinned header row, a rule,
/// then as many rows as fit.
fn draw_grid(tab: &mut Tab, colors: &Colors, area: Rect, buf: &mut Buffer, g: &mut Geometry) {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let (top_origin, left_origin) = tab.sheet.origin;
    let last_row = top_origin + tab.sheet.rows.len();
    let gutter = last_row.to_string().len() as u16 + 2;
    g.gutter = area.x + gutter;
    let bar_y = area.y;
    g.letters_y = area.y + 1;
    g.header_y = area.y + 2;
    let rule_y = area.y + 3;
    g.body_y = area.y + 4;
    g.body_rows = g.status_y.saturating_sub(g.body_y + 1);
    let body_rows = usize::from(g.body_rows.max(1));

    // Scroll to the cursor after it moved.
    if tab.follow {
        if tab.cursor.row > 0 {
            let line = tab.cursor.row - 1;
            if line < tab.top {
                tab.top = line;
            } else if line >= tab.top + body_rows {
                tab.top = line + 1 - body_rows;
            }
        }
        if tab.cursor.col < tab.left {
            tab.left = tab.cursor.col;
        }
        let room = area.width.saturating_sub(gutter);
        let span =
            |from: usize, to: usize| -> u16 { tab.widths[from..=to].iter().map(|w| w + 3).sum() };
        while tab.left < tab.cursor.col && span(tab.left, tab.cursor.col) > room {
            tab.left += 1;
        }
    }
    tab.top = tab.top.min(tab.order.len().saturating_sub(1));

    // The cell bar: where the cursor is, and its whole text or formula.
    let cursor = tab.cursor;
    let address = tab.address(cursor);
    buf.set_string(area.x + 1, bar_y, &address, colors.accent);
    let content = match tab.formula(cursor) {
        Some(formula) => format!("{formula}  →  {}", one_line(tab.cell(cursor))),
        None => one_line(tab.cell(cursor)),
    };
    let bar_x = area.x + 1 + address.width() as u16 + 2;
    buf.set_string(
        bar_x,
        bar_y,
        fit(
            &content,
            usize::from(area.right().saturating_sub(bar_x + 1)),
        ),
        Style::default(),
    );

    // Columns that fit, from `left`.
    let mut x = area.x + gutter;
    for col in tab.left..tab.columns() {
        if x >= area.right() {
            break;
        }
        let w = tab.widths[col];
        let end = (x + w + 1).min(area.right() - 1);
        g.columns.push((x, end, col));
        x = end + 2;
    }
    let (sel_from, sel_to) = tab.selection();
    let selecting = tab.anchor.is_some();

    // Column letters, the cursor's in the accent.
    for &(from, to, col) in &g.columns {
        let label = letters(left_origin + col);
        let w = usize::from(to - from + 1);
        let text = format!("{label:^w$}");
        let style = if col == cursor.col {
            colors.accent
        } else if selecting && (sel_from.col..=sel_to.col).contains(&col) {
            bold
        } else {
            dim
        };
        buf.set_string(from, g.letters_y, fit(&text, w), style);
    }

    let draw_row = |buf: &mut Buffer, y: u16, line: usize, tab: &Tab| {
        let source = tab.source(line).unwrap_or(0);
        let number = (top_origin + source + 1).to_string();
        let number_style = if line == cursor.row {
            colors.accent
        } else {
            dim
        };
        buf.set_string(
            area.x + gutter - 1 - number.len() as u16,
            y,
            &number,
            number_style,
        );
        for (i, &(from, to, col)) in g.columns.iter().enumerate() {
            let pos = Pos { row: line, col };
            let w = usize::from(to - from + 1);
            let text = one_line(tab.cell(pos));
            let inner = w.saturating_sub(2);
            let shown = fit(&text, inner);
            let pad = inner.saturating_sub(shown.width());
            let numeric = if line == 0 {
                tab.numeric.get(col).copied().unwrap_or(false)
            } else {
                value(&text).is_some()
            };
            let padded = if numeric {
                format!(" {}{shown} ", " ".repeat(pad))
            } else {
                format!(" {shown}{} ", " ".repeat(pad))
            };
            let mut style = if line == 0 { bold } else { Style::default() };
            if ERRORS.contains(&text.as_str()) {
                style = style.fg(Color::Indexed(1));
            }
            if pos == cursor {
                style = style.patch(colors.cursor);
            } else if selecting && tab.selected(pos) {
                style = style.patch(colors.selection);
            }
            buf.set_string(from, y, fit(&padded, w), style);
            if i + 1 < g.columns.len() || to + 1 < area.right() {
                buf.set_string(to + 1, y, "│", dim);
            }
        }
    };

    draw_row(buf, g.header_y, 0, tab);
    // The rule under the header, crossing at each column.
    let rule_end = g
        .columns
        .last()
        .map_or(area.x + gutter, |&(_, to, _)| to + 1);
    for col in area.x + gutter.saturating_sub(1)..=rule_end.min(area.right() - 1) {
        let crossing = g.columns.iter().any(|&(_, to, _)| to + 1 == col);
        buf.set_string(col, rule_y, if crossing { "┼" } else { "─" }, dim);
    }
    for i in 0..body_rows {
        let line = tab.top + i + 1;
        if line >= tab.lines() {
            break;
        }
        draw_row(buf, g.body_y + i as u16, line, tab);
    }
}

pub fn run(path: &Path, keys: Keymap<SheetAction>) -> Result<(), SheetViewFailed> {
    let sheets = sheet::load(path)?;
    let title = path
        .file_name()
        .map_or_else(|| "sheet".into(), |n| n.to_string_lossy().into_owned());
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    // Asked in raw mode, so the reply is read here byte by byte. Asked
    // before, it arrived late and was read as typed keys.
    let colors = Colors::of(Theme::query());
    let mut app = App::new(title.clone(), sheets, keys, colors);
    let result = execute!(io::stdout(), EnableMouseCapture, SetTitle(&title))
        .and_then(|()| event_loop(&mut app, &mut terminal));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    Ok(result?)
}

fn event_loop(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    loop {
        terminal.draw(|f| {
            let area = f.area();
            app.draw(area, f.buffer_mut());
        })?;
        if let Some(text) = app.copy.take() {
            let encoded = base64::engine::general_purpose::STANDARD.encode(text);
            let mut out = io::stdout();
            // A terminal that refuses the clipboard write only loses the copy.
            let _ = write!(out, "\x1b]52;c;{encoded}\x07").and_then(|()| out.flush());
        }
        if app.quit {
            return Ok(());
        }
        // Takes every waiting event before drawing again, so a fast drag
        // paints once per batch.
        let mut event = event::read()?;
        loop {
            match event {
                Event::Key(key) => app.key(key),
                Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved => {}
                Event::Mouse(mouse) => app.mouse(mouse),
                _ => {}
            }
            if app.quit || !event::poll(std::time::Duration::ZERO)? {
                break;
            }
            event = event::read()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet(rows: &[&[&str]]) -> Sheet {
        Sheet {
            name: "S".into(),
            origin: (0, 0),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
                .collect(),
            formulas: Default::default(),
        }
    }

    fn people() -> Sheet {
        sheet(&[
            &["Name", "Age", "City"],
            &["Ada", "36", "London"],
            &["Grace", "85", "New York"],
            &["Linus", "1,200", "Helsinki"],
            &["Alan", "", "Wilmslow"],
        ])
    }

    fn app(sheets: Vec<Sheet>) -> App {
        App::new(
            "people.csv".into(),
            sheets,
            Keymap::default(),
            Colors::of(None),
        )
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            app.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        app.draw(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The screen column where `needle` starts in a drawn row.
    fn column_of(row: &str, needle: &str) -> u16 {
        let at = row.find(needle).expect("drawn");
        row[..at].width() as u16
    }

    #[test]
    fn numbers_read_through_separators_currency_and_percent() {
        assert_eq!(value("1,234.5"), Some(1234.5));
        assert_eq!(value("$12"), Some(12.0));
        assert_eq!(value("50%"), Some(0.5));
        assert_eq!(value("-3"), Some(-3.0));
        assert_eq!(value("-$4.50"), Some(-4.5));
        assert_eq!(value("inf"), None);
        assert_eq!(value("1e3"), Some(1000.0));
        assert_eq!(value("12,34"), None);
        assert_eq!(value("Ada"), None);
        assert_eq!(value(""), None);
    }

    #[test]
    fn the_grid_shows_letters_numbers_the_pinned_header_and_rows() {
        let mut app = app(vec![people()]);
        let rows = screen(&mut app, 60, 10);
        assert_eq!(rows[0], " A1  Name");
        assert!(rows[1].contains('A') && rows[1].contains('B') && rows[1].contains('C'));
        assert!(rows[2].starts_with(" 1  Name"), "{rows:?}");
        assert!(rows[3].contains('┼'));
        assert!(rows[4].contains("Ada") && rows[4].trim_start().starts_with('2'));
        assert!(rows[6].contains("1,200"));
        assert!(rows[9].contains("people.csv") && rows[9].contains("4 rows"));
    }

    #[test]
    fn numbers_sit_at_the_right_of_their_column() {
        let mut app = app(vec![people()]);
        let rows = screen(&mut app, 60, 10);
        // "Age" is 3 wide but "1,200" is 5: 36 is pushed right.
        let ada = &rows[4];
        let age_at = ada.find("36").unwrap();
        let linus_at = rows[6].find("1,200").unwrap();
        assert_eq!(age_at + 2, linus_at + 5);
        // The header of a column of numbers lines up with them too.
        assert_eq!(
            column_of(&rows[2], "Age") + 3,
            column_of(&rows[6], "1,200") + 5
        );
        assert_eq!(column_of(&rows[2], "Name"), column_of(&rows[4], "Ada"));
    }

    #[test]
    fn keys_move_the_cursor_and_the_cell_bar_follows() {
        let mut app = app(vec![people()]);
        press(&mut app, "jjl");
        let rows = screen(&mut app, 60, 10);
        assert_eq!(rows[0], " B3  85");
        press(&mut app, "G$");
        let rows = screen(&mut app, 60, 10);
        assert_eq!(rows[0], " C5  Wilmslow");
        press(&mut app, "g0");
        assert_eq!(screen(&mut app, 60, 10)[0], " A1  Name");
    }

    #[test]
    fn a_selection_totals_its_numbers_and_copies_as_tsv() {
        let mut app = app(vec![people()]);
        press(&mut app, "ljJJJ");
        let rows = screen(&mut app, 80, 10);
        assert!(
            rows[9].contains("Count 3 · Sum 1,321 · Average 440.33 ·"),
            "{}",
            rows[9]
        );
        press(&mut app, "y");
        assert_eq!(app.copy.as_deref(), Some("36\n85\n1,200\n"));
        assert!(screen(&mut app, 80, 10)[9].contains("Copied 4 cells."));
    }

    #[test]
    fn sorting_cycles_up_down_and_back_keeping_empty_cells_last() {
        let mut tab = Tab::new(people());
        tab.cursor.col = 1;
        tab.cycle_sort();
        let names = |tab: &Tab| -> Vec<String> {
            tab.order
                .iter()
                .map(|&r| tab.sheet.rows[r][0].clone())
                .collect()
        };
        assert_eq!(names(&tab), ["Ada", "Grace", "Linus", "Alan"]);
        tab.cycle_sort();
        assert_eq!(names(&tab), ["Linus", "Grace", "Ada", "Alan"]);
        tab.cycle_sort();
        assert_eq!(names(&tab), ["Ada", "Grace", "Linus", "Alan"]);
        assert_eq!(tab.sort, None);
    }

    #[test]
    fn sorting_keeps_the_cursor_on_its_row() {
        let mut tab = Tab::new(people());
        tab.go(Pos { row: 3, col: 1 }, false);
        assert_eq!(tab.cell(tab.cursor), "1,200");
        tab.cycle_sort();
        tab.cycle_sort();
        assert_eq!(tab.cell(tab.cursor), "1,200");
        assert_eq!(tab.cursor.row, 1);
    }

    #[test]
    fn a_filter_shows_matching_rows_and_esc_brings_back_the_rest() {
        let mut app = app(vec![people()]);
        press(&mut app, "llFo\n");
        let tab = &app.tabs[0];
        assert_eq!(tab.order, [1, 2, 4]);
        let rows = screen(&mut app, 80, 10);
        assert!(
            rows[9].contains("3 of 4 rows") && rows[9].contains("C holds \"o\""),
            "{}",
            rows[9]
        );
        press(&mut app, "\x1b");
        assert_eq!(app.tabs[0].order.len(), 4);
    }

    #[test]
    fn find_wraps_around_and_n_repeats_it() {
        let mut app = app(vec![people()]);
        press(&mut app, "/l\n");
        assert_eq!(app.tabs[0].cursor, Pos { row: 1, col: 2 }, "London");
        press(&mut app, "n");
        assert_eq!(app.tabs[0].cursor, Pos { row: 3, col: 0 }, "Linus");
        press(&mut app, "n");
        assert_eq!(app.tabs[0].cursor, Pos { row: 3, col: 2 }, "Helsinki");
        press(&mut app, "N");
        assert_eq!(app.tabs[0].cursor, Pos { row: 3, col: 0 }, "Linus");
        press(&mut app, "G$n");
        assert_eq!(
            app.tabs[0].cursor,
            Pos { row: 1, col: 2 },
            "wraps to London"
        );
        press(&mut app, "/zzz\n");
        assert!(screen(&mut app, 80, 10)[9].contains("No cell holds \"zzz\"."));
    }

    #[test]
    fn go_to_takes_addresses_in_the_files_own_letters_and_numbers() {
        let mut sheet = people();
        sheet.origin = (2, 1);
        let mut tab = Tab::new(sheet);
        assert_eq!(tab.address(Pos { row: 0, col: 0 }), "B3");
        assert_eq!(tab.locate("c5"), Ok(Pos { row: 2, col: 1 }));
        assert_eq!(tab.locate("4"), Ok(Pos { row: 1, col: 0 }));
        assert!(tab.locate("A1").unwrap_err().contains("column A"));
        assert!(tab.locate("B99").unwrap_err().contains("row 99"));
        tab.filter = Some((0, "Grace".into()));
        tab.arrange();
        assert!(
            tab.locate("B4")
                .unwrap_err()
                .contains("hidden by the filter")
        );
    }

    #[test]
    fn clicking_a_cell_moves_there_and_dragging_selects() {
        let mut app = app(vec![people()]);
        let rows = screen(&mut app, 80, 10);
        let x = column_of(&rows[5], "Grace");
        let click = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.mouse(click(MouseEventKind::Down(MouseButton::Left), x, 5));
        assert_eq!(app.tabs[0].cursor, Pos { row: 2, col: 0 });
        let age = column_of(&rows[6], "1,200");
        app.mouse(click(MouseEventKind::Drag(MouseButton::Left), age, 6));
        app.mouse(click(MouseEventKind::Up(MouseButton::Left), age, 6));
        assert_eq!(
            app.tabs[0].selection(),
            (Pos { row: 2, col: 0 }, Pos { row: 3, col: 1 })
        );
    }

    #[test]
    fn clicking_a_column_letter_selects_the_column() {
        let mut app = app(vec![people()]);
        screen(&mut app, 80, 10);
        let (from, _, _) = app.geometry.columns[1];
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: from + 1,
            row: app.geometry.letters_y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.tabs[0].selection(),
            (Pos { row: 0, col: 1 }, Pos { row: 4, col: 1 })
        );
    }

    #[test]
    fn sheets_switch_with_brackets_and_by_clicking_their_tab() {
        let mut second = people();
        second.name = "Other".into();
        let mut app = app(vec![people(), second]);
        let rows = screen(&mut app, 80, 10);
        assert!(rows[9].contains(" S ") && rows[9].contains(" Other "));
        press(&mut app, "]");
        assert_eq!(app.current, 1);
        press(&mut app, "]");
        assert_eq!(app.current, 0);
        screen(&mut app, 80, 10);
        let (x, _, _) = app.geometry.tabs[1];
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: 9,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.current, 1);
    }

    #[test]
    fn the_view_scrolls_to_keep_the_cursor_on_screen() {
        let rows: Vec<Vec<String>> = (0..100)
            .map(|r| vec![format!("r{r}"), format!("{r}")])
            .collect();
        let mut app = app(vec![Sheet {
            name: String::new(),
            origin: (0, 0),
            rows,
            formulas: Default::default(),
        }]);
        press(&mut app, "G");
        let rows = screen(&mut app, 40, 12);
        assert!(rows[9].contains("r99"), "{rows:?}");
        assert!(rows[2].contains("r0"), "the header stays pinned");
    }

    #[test]
    fn formulas_show_in_the_cell_bar() {
        let mut sheet = people();
        sheet.formulas.insert((1, 1), "=30+6".into());
        let mut app = app(vec![sheet]);
        press(&mut app, "jl");
        assert_eq!(screen(&mut app, 60, 10)[0], " B2  =30+6  →  36");
    }
}
