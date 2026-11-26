//! The diff reviewer: a virtualized patch view, line selection, comments, and
//! sending them to an agent or a pull request.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::{self, TryRecvError};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use ratatui::Frame;
use ratatui::buffer::CellWidth;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::agent;
use crate::art;
use crate::comment::{Anchor, Comment, Side, State};
use crate::diff::{self, Diff, File, Kind as RowKind, Loaded, Request};
use crate::diffkeys::DiffAction;
use crate::diffreview::{self, FileNotes, Margin, Reopen, SaveNotes, Sends};
use crate::github;
use crate::keymap::Keymap;
use crate::linediff;
use crate::look::Look;
use crate::news::News;
use crate::outline::{self, Ask, OpenOutline, OutlineForm};
use crate::progress::{SavedComment, Store, StoreFailed};
use crate::remote;
use crate::render::DIM;
use crate::review::{Outcome, Verdict};
use crate::scheme;
use crate::syntax;
use crate::theme::{self, Theme};
use crate::usage;

const ACCENT: Color = Color::Indexed(12);
const ADDED_FG: Color = Color::Indexed(2);
const REMOVED_FG: Color = Color::Indexed(1);
/// How many hunks may be lexed in one frame; the rest stay plain until the
/// reader pauses. Keeps a fast scroll from spending the frame in the lexer.
const HIGHLIGHT_BUDGET: u8 = 1;
/// Hunks kept lexed before the whole cache is dropped.
const HIGHLIGHT_CACHE: usize = 64;
/// A hunk longer than this stays plain; lexing it would cost more than it adds.
const HIGHLIGHT_BYTES: usize = 128 * 1024;
/// How many rows one wheel notch moves.
const WHEEL_ROWS: usize = 3;
const OUTLINE_COLUMNS: u16 = 32;
const OUTLINE_MIN_SCREEN: u16 = OUTLINE_COLUMNS + 40;

/// Line tints: the theme's own green, red and blue mixed over its background
/// when the theme is known, otherwise fixed shades. Exact RGB either way, not
/// the 256-color palette: some terminals derive that palette from the theme,
/// which turns the light shades dark on a light background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    added: Color,
    removed: Color,
    added_word: Color,
    removed_word: Color,
    focus: Color,
    /// The cursor line: a faint wash on unchanged code, and stronger
    /// added and removed tints so a changed line keeps its color.
    cursor: Color,
    added_cursor: Color,
    removed_cursor: Color,
}

/// How strongly each tint shows over the theme's background.
const LINE_TINT: f32 = 0.2;
const WORD_TINT: f32 = 0.4;
const FOCUS_TINT: f32 = 0.3;
const CURSOR_TINT: f32 = 0.12;
const CHANGED_CURSOR_TINT: f32 = 0.3;

impl Palette {
    fn new(light: bool) -> Self {
        if light {
            Palette::light()
        } else {
            Palette::dark()
        }
    }

    fn dark() -> Self {
        Palette {
            added: Color::Rgb(0x00, 0x5f, 0x00),
            removed: Color::Rgb(0x5f, 0x00, 0x00),
            added_word: Color::Rgb(0x00, 0x87, 0x00),
            removed_word: Color::Rgb(0x87, 0x00, 0x00),
            focus: Color::Rgb(0x00, 0x5f, 0x87),
            cursor: Color::Rgb(0x1c, 0x26, 0x30),
            added_cursor: Color::Rgb(0x00, 0x73, 0x00),
            removed_cursor: Color::Rgb(0x73, 0x00, 0x00),
        }
    }

    fn light() -> Self {
        Palette {
            added: Color::Rgb(0xd7, 0xff, 0xd7),
            removed: Color::Rgb(0xff, 0xd7, 0xd7),
            added_word: Color::Rgb(0xaf, 0xff, 0xaf),
            removed_word: Color::Rgb(0xff, 0xaf, 0xaf),
            focus: Color::Rgb(0xaf, 0xd7, 0xff),
            cursor: Color::Rgb(0xea, 0xf2, 0xfb),
            added_cursor: Color::Rgb(0xc3, 0xff, 0xc3),
            removed_cursor: Color::Rgb(0xff, 0xc3, 0xc3),
        }
    }

    fn from_theme(theme: Theme) -> Self {
        let tint = |color, amount| {
            let [r, g, b] = theme::mix(theme.background, color, amount);
            Color::Rgb(r, g, b)
        };
        Palette {
            added: tint(theme.green, LINE_TINT),
            removed: tint(theme.red, LINE_TINT),
            added_word: tint(theme.green, WORD_TINT),
            removed_word: tint(theme.red, WORD_TINT),
            focus: tint(theme.blue, FOCUS_TINT),
            cursor: tint(theme.blue, CURSOR_TINT),
            added_cursor: tint(theme.green, CHANGED_CURSOR_TINT),
            removed_cursor: tint(theme.red, CHANGED_CURSOR_TINT),
        }
    }

    /// The terminal's colors when it gave them, else the fixed shades.
    fn of_terminal(theme: Option<Theme>, light: bool) -> Self {
        theme.map_or_else(|| Palette::new(light), Palette::from_theme)
    }

    fn of(colors: scheme::Colors) -> Self {
        Palette::of_terminal(colors.theme, colors.light)
    }
}

/// Comments and the store they persist in.
pub struct Options {
    pub comments: Vec<SavedComment>,
    pub store: Option<Store>,
    /// Store failures are reported after the terminal is restored.
    pub failure: Rc<RefCell<Option<String>>>,
    /// Opened as a pull request's Code tab: `1`, `2`, `3` and `5` go to its
    /// other tabs, and `h`/`l` scroll sideways.
    pub tabs: bool,
    pub layout: Layout,
    /// The keys the diff answers to, as `[keys.diff]` sets them.
    pub keys: Keymap<DiffAction>,
    /// File and global notes, and where `S` sends the review.
    pub margin: Margin,
}

/// How changed lines are laid out; `i` switches between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Old code on the left, new code on the right.
    #[default]
    Split,
    /// One column, removed lines above added ones, as `git diff` shows them.
    Inline,
}

impl Layout {
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Split => "split",
            Layout::Inline => "inline",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "split" => Some(Layout::Split),
            "inline" => Some(Layout::Inline),
            _ => None,
        }
    }

    fn pane_widths(self, width: u16) -> (u16, u16) {
        match self {
            Layout::Inline => (width, width),
            Layout::Split => {
                let space = width.saturating_sub(1); // split divider
                (space / 2, space - space / 2)
            }
        }
    }

    fn gutter(self, number: usize) -> usize {
        match self {
            Layout::Split => number + 2,
            Layout::Inline => 2 * number + 3,
        }
    }

    fn toggled(self) -> Self {
        match self {
            Layout::Split => Layout::Inline,
            Layout::Inline => Layout::Split,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            comments: Vec::new(),
            store: None,
            failure: Rc::new(RefCell::new(None)),
            tabs: false,
            layout: Layout::Split,
            keys: Keymap::default(),
            margin: Margin::default(),
        }
    }
}

/// How the reviewer was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leave {
    Quit,
    /// For a pull request's Code tab: to its Summary, Timeline, Comments or Checks.
    Summary,
    Timeline,
    Comments,
    Checks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogKind {
    Help,
    Comments,
    Files,
    Error,
    Confirm,
    Loading,
    Verdict,
    Outline,
}

struct Confirm {
    body: String,
    anchor: Anchor,
    composer: Composer,
    /// The saved comment being posted again, when it isn't new.
    edit: Option<usize>,
}

enum Dialog {
    None,
    Help,
    Comments {
        selected: usize,
    },
    Files {
        /// The highlighted row, counted in filtered files.
        selected: usize,
        /// What has been typed into the search bar.
        query: String,
    },
    Error(String),
    Confirm(Confirm),
    Loading(String),
    /// Picking the verdict the review goes out with.
    Verdict,
    /// The tree stays in `outline`; this only holds the keys.
    Outline,
}

impl Dialog {
    fn kind(&self) -> DialogKind {
        match self {
            Dialog::None => unreachable!(),
            Dialog::Help => DialogKind::Help,
            Dialog::Comments { .. } => DialogKind::Comments,
            Dialog::Files { .. } => DialogKind::Files,
            Dialog::Error(_) => DialogKind::Error,
            Dialog::Confirm(_) => DialogKind::Confirm,
            Dialog::Loading(_) => DialogKind::Loading,
            Dialog::Verdict => DialogKind::Verdict,
            Dialog::Outline => DialogKind::Outline,
        }
    }
}

struct Toast {
    text: String,
    until: Instant,
    error: bool,
}

impl Toast {
    fn message(text: impl Into<String>) -> Self {
        Toast {
            text: text.into(),
            until: Instant::now() + Duration::from_secs(4),
            error: false,
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Toast {
            text: text.into(),
            until: Instant::now() + Duration::from_secs(8),
            error: true,
        }
    }
}

/// What enter does with the text written in the box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    /// Saves it as a review comment (`C`).
    Comment,
    /// Sends it to the agent pane as a question about the lines (`@`).
    Ask,
}

/// The comment being written, and the lines it hangs off.
#[derive(Clone)]
struct Composer {
    text: Vec<char>,
    cursor: usize,
    anchor: Anchor,
    first: usize,
    last: usize,
    purpose: Purpose,
}

enum Job {
    Load {
        rx: mpsc::Receiver<Result<Loaded, diff::DiffFailed>>,
        remember: Option<(String, Side, u32)>,
        background: bool,
    },
    Post {
        rx: mpsc::Receiver<Result<String, github::GithubFailed>>,
        index: usize,
        composer: Composer,
        added: bool,
    },
}

enum Done {
    Load {
        result: Box<Result<Loaded, diff::DiffFailed>>,
        remember: Option<(String, Side, u32)>,
        background: bool,
    },
    Post {
        result: Result<String, github::GithubFailed>,
        index: usize,
        composer: Composer,
        added: bool,
    },
}

/// Sends comment text to an agent, returning the pane it reached.
type Deliver = Box<dyn FnMut(&str) -> Result<agent::Agent, agent::AgentFailed>>;

/// One line on screen. In the split layout changed blocks are paired
/// left-to-right; inline, each line holds one changed row, or a context row on
/// both sides. Headings and notes span the width. A folded file is its
/// heading alone, with `folded` set.
#[derive(Debug, Clone, Copy)]
struct SplitRow {
    left: Option<usize>,
    right: Option<usize>,
    band: Option<usize>,
    folded: bool,
}

/// What a split line is going through this frame.
#[derive(Clone, Copy)]
struct SideState {
    current: bool,
    highlighted: bool,
}

impl SplitRow {
    /// A line of code, on either side.
    fn is_content(self) -> bool {
        self.left.is_some() || self.right.is_some()
    }

    /// A line the cursor can rest on: code, or a folded file's heading.
    fn is_stop(self) -> bool {
        self.is_content() || self.folded
    }

    /// The side the cursor and a new comment prefer: the new side.
    fn focus_row(self) -> Option<usize> {
        self.right.or(self.left).or(self.band)
    }

    fn rows(self) -> impl Iterator<Item = usize> {
        [self.left, self.right, self.band].into_iter().flatten()
    }

    fn first_row(self) -> Option<usize> {
        self.rows().min()
    }

    fn last_row(self) -> Option<usize> {
        self.rows().max()
    }
}

/// The split view's lines, and the line each diff row is drawn on.
struct Split {
    lines: Vec<SplitRow>,
    line_of_row: Vec<usize>,
    /// For a change block whose lines don't pair in Git's order, its first
    /// removed row and how far its shorter side shifts (see `linediff::alignment`).
    shifts: HashMap<usize, usize>,
}

/// The removed rows and the added rows of the change block holding `row`,
/// which must be a removed or added row inside `hunk`.
fn change_block(
    rows: &[diff::Row],
    hunk: &diff::Hunk,
    row: usize,
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let is = |at: usize, kind: RowKind| rows.get(at).is_some_and(|r| r.kind == kind);
    let mut first = row;
    if is(row, RowKind::Added) {
        while first > hunk.first && is(first - 1, RowKind::Added) {
            first -= 1;
        }
    } else if !is(row, RowKind::Removed) {
        return None;
    }
    while first > hunk.first && is(first - 1, RowKind::Removed) {
        first -= 1;
    }
    let mut middle = first;
    while middle < hunk.end && is(middle, RowKind::Removed) {
        middle += 1;
    }
    let mut end = middle;
    while end < hunk.end && is(end, RowKind::Added) {
        end += 1;
    }
    Some((first..middle, middle..end))
}

/// Each change block with removed and added rows that pair better shifted,
/// by its first removed row.
fn block_shifts(diff: &Diff) -> HashMap<usize, usize> {
    let rows = diff.rows();
    let mut shifts = HashMap::new();
    let mut at = 0;
    while at < rows.len() {
        if rows[at].kind != RowKind::Removed {
            at += 1;
            continue;
        }
        let first = at;
        while at < rows.len() && rows[at].kind == RowKind::Removed {
            at += 1;
        }
        let middle = at;
        while at < rows.len() && rows[at].kind == RowKind::Added {
            at += 1;
        }
        let old: Vec<&str> = (first..middle).map(|row| diff.content(row)).collect();
        let new: Vec<&str> = (middle..at).map(|row| diff.content(row)).collect();
        let shift = linediff::alignment(&old, &new);
        if shift > 0 {
            shifts.insert(first, shift);
        }
    }
    shifts
}

impl Split {
    /// The row `row` is drawn beside and compared with: the added row paired
    /// with a removed one, or the other way round.
    fn partner(&self, diff: &Diff, row: usize) -> Option<usize> {
        let rows = diff.rows();
        let r = rows.get(row)?;
        let hunk = diff.file(r.file as usize)?.hunks.get(r.hunk as usize)?;
        let (old, new) = change_block(rows, hunk, row)?;
        let shift = self.shifts.get(&old.start).copied().unwrap_or(0);
        let new_longer = new.len() > old.len();
        if r.kind == RowKind::Removed {
            let k = row - old.start;
            let index = if new_longer {
                Some(k + shift)
            } else {
                k.checked_sub(shift)
            };
            index.filter(|&i| i < new.len()).map(|i| new.start + i)
        } else {
            let k = row - new.start;
            let index = if new_longer {
                k.checked_sub(shift)
            } else {
                Some(k + shift)
            };
            index.filter(|&i| i < old.len()).map(|i| old.start + i)
        }
    }

    /// The lines for `diff`, with the files in `folded` shown as their
    /// heading alone.
    fn new(diff: &Diff, layout: Layout, folded: &HashSet<usize>) -> Self {
        let rows = diff.rows();
        let shifts = block_shifts(diff);
        let mut lines = Vec::new();
        let mut line_of_row = vec![0; rows.len()];
        let mut push = |line: SplitRow, line_of_row: &mut Vec<usize>| {
            let index = lines.len();
            for row in line.rows() {
                if let Some(slot) = line_of_row.get_mut(row) {
                    *slot = index;
                }
            }
            lines.push(line);
        };
        let mut at = 0;
        while at < rows.len() {
            match rows[at].kind {
                RowKind::File if folded.contains(&(rows[at].file as usize)) => {
                    push(
                        SplitRow {
                            left: None,
                            right: None,
                            band: Some(at),
                            folded: true,
                        },
                        &mut line_of_row,
                    );
                    // The file's rows all land on its heading.
                    let line = line_of_row[at];
                    at += 1;
                    while at < rows.len() && rows[at].kind != RowKind::File {
                        line_of_row[at] = line;
                        at += 1;
                    }
                }
                RowKind::Context => {
                    push(
                        SplitRow {
                            left: Some(at),
                            right: Some(at),
                            band: None,
                            folded: false,
                        },
                        &mut line_of_row,
                    );
                    at += 1;
                }
                RowKind::Removed if layout == Layout::Inline => {
                    push(
                        SplitRow {
                            left: Some(at),
                            right: None,
                            band: None,
                            folded: false,
                        },
                        &mut line_of_row,
                    );
                    at += 1;
                }
                RowKind::Removed => {
                    let old_start = at;
                    while at < rows.len() && rows[at].kind == RowKind::Removed {
                        at += 1;
                    }
                    let new_start = at;
                    while at < rows.len() && rows[at].kind == RowKind::Added {
                        at += 1;
                    }
                    let old = new_start - old_start;
                    let new = at - new_start;
                    let shift = shifts.get(&old_start).copied().unwrap_or(0);
                    let (left_shift, right_shift) = if new > old { (shift, 0) } else { (0, shift) };
                    for pair in 0..old.max(new) {
                        let side = |shift: usize, len: usize, start: usize| {
                            pair.checked_sub(shift)
                                .filter(|&i| i < len)
                                .map(|i| start + i)
                        };
                        push(
                            SplitRow {
                                left: side(left_shift, old, old_start),
                                right: side(right_shift, new, new_start),
                                band: None,
                                folded: false,
                            },
                            &mut line_of_row,
                        );
                    }
                }
                RowKind::Added => {
                    let start = at;
                    while at < rows.len() && rows[at].kind == RowKind::Added {
                        at += 1;
                    }
                    for row in start..at {
                        push(
                            SplitRow {
                                left: None,
                                right: Some(row),
                                band: None,
                                folded: false,
                            },
                            &mut line_of_row,
                        );
                    }
                }
                RowKind::File | RowKind::Hunk | RowKind::Meta | RowKind::NoNewline => {
                    push(
                        SplitRow {
                            left: None,
                            right: None,
                            band: Some(at),
                            folded: false,
                        },
                        &mut line_of_row,
                    );
                    at += 1;
                }
            }
        }
        Split {
            lines,
            line_of_row,
            shifts,
        }
    }

    fn lines(&self) -> &[SplitRow] {
        &self.lines
    }

    fn line_for_row(&self, row: usize) -> Option<usize> {
        self.line_of_row.get(row).copied()
    }

    /// The display lines for a pair of diff rows, in reading order.
    fn lines_for_rows(&self, first: usize, last: usize) -> Option<(usize, usize)> {
        let first = self.line_for_row(first)?;
        let last = self.line_for_row(last)?;
        Some((first.min(last), first.max(last)))
    }

    /// The diff rows a range of display lines covers.
    fn rows_between(&self, first: usize, last: usize) -> Option<(usize, usize)> {
        let (first, last) = (first.min(last), first.max(last));
        let mut rows = self.lines.get(first..=last)?.iter().flat_map(|line| {
            let start = line.first_row();
            let end = line.last_row();
            start.into_iter().chain(end)
        });
        let first = rows.next()?;
        Some(rows.fold((first, first), |(low, high), row| {
            (low.min(row), high.max(row))
        }))
    }

    /// The diff rows a comment on display lines `first..=last` covers. When
    /// the rows between the lines' first and last reach a line outside them,
    /// as a paired line's two rows do once a block's pairing shifts, it is
    /// the new side's rows alone, or the old side's when no line shows a new one.
    fn comment_rows(&self, first: usize, last: usize) -> Option<(usize, usize)> {
        let (first, last) = (first.min(last), first.max(last));
        let all = self.rows_between(first, last)?;
        let inside = (all.0..=all.1).all(|row| {
            self.line_for_row(row)
                .is_some_and(|line| (first..=last).contains(&line))
        });
        if inside {
            return Some(all);
        }
        let lines = self.lines.get(first..=last)?;
        let span = |rows: Vec<usize>| Some((*rows.iter().min()?, *rows.iter().max()?));
        span(lines.iter().filter_map(|line| line.right).collect())
            .or_else(|| span(lines.iter().filter_map(|line| line.left).collect()))
    }

    fn first_content(&self) -> Option<usize> {
        self.lines.iter().position(|line| line.is_stop())
    }

    fn last_content(&self) -> Option<usize> {
        self.lines.iter().rposition(|line| line.is_stop())
    }

    fn next_content(&self, line: usize, forward: bool) -> Option<usize> {
        if forward {
            (line + 1..self.lines.len()).find(|&i| self.lines[i].is_stop())
        } else {
            (0..line.min(self.lines.len()))
                .rev()
                .find(|&i| self.lines[i].is_stop())
        }
    }

    fn snap(&self, line: usize) -> usize {
        if self.lines.get(line).is_some_and(|line| line.is_stop()) {
            return line;
        }
        self.next_content(line, true)
            .or_else(|| self.next_content(line, false))
            .unwrap_or(line.min(self.lines.len().saturating_sub(1)))
    }
}

struct App {
    loaded: Loaded,
    palette: Palette,
    /// Time spent here, for the home screen's heat map.
    clock: usage::Clock,
    look: Look,
    /// This version's notes, up once after an update until OK.
    news: Option<News>,
    /// When the last key, click or paste came, so the terminal is asked
    /// about its theme only once the reader has paused.
    last_input: Instant,
    polled: Instant,
    stale: bool,
    /// The split line the cursor is on.
    cursor: usize,
    /// First split line on screen.
    scroll: usize,
    left: usize,
    /// Maximum old/new content widths, measured only on the first pan in a diff.
    content_widths: Option<(usize, usize)>,
    /// The split line where a visual selection started.
    selection: Option<usize>,
    /// Vim-style visual mode: movement extends the selection, and Escape leaves
    /// visual mode and drops the range.
    visual: bool,
    /// The split lines of the comment just jumped to, highlighted until the
    /// reader moves.
    focus: Option<(usize, usize)>,
    drag_origin: Option<usize>,
    composer: Option<Composer>,
    /// A command from Rex's command palette, being typed in after F12.
    rex_command: Option<String>,
    dialog: Dialog,
    comments: Vec<SavedComment>,
    /// Rows a saved comment marks, and its state, for the gutter.
    marks: HashMap<usize, State>,
    store: Option<Store>,
    failure: Rc<RefCell<Option<String>>>,
    key: String,
    toast: Option<Toast>,
    job: Option<Job>,
    copy: Option<String>,
    quit: bool,
    /// Where `quit` goes, for a pull request's Code tab.
    leave: Leave,
    tabs: bool,
    keys: Keymap<DiffAction>,
    layout: Layout,
    split: Split,
    /// The paths of the files shown as their heading alone.
    folded: HashSet<String>,
    /// The files marked viewed, kept so headings don't fingerprint a file
    /// every frame.
    viewed: HashSet<usize>,
    /// The diff's lines on screen.
    view: Rect,
    /// Rows inside the comment box, for moving the cursor.
    composer_view: u16,
    highlight: HashMap<(u32, u32), Rc<Vec<Vec<syntax::Kind>>>>,
    new_highlights: u8,
    deliver: Deliver,
    /// File and global notes and viewed marks.
    notes: FileNotes,
    save_notes: SaveNotes,
    reopen_notes: Option<Reopen>,
    sends: Sends,
    /// The overview of every file, with the row picked: 0 is the global
    /// note, then one row per file. `None` while reading a file.
    overview: Option<usize>,
    outline: Option<OpenOutline>,
    /// How wide the outline's sidebar is asked to be; `<` and `>` change it for the session.
    outline_columns: u16,
    note_editor: Option<NoteEditor>,
    /// How the review ended, for `leaf review`.
    outcome: Option<Outcome>,
    /// Set by a first `q` with notes unsent, so a second one quits.
    quit_armed: bool,
}

/// A file or global note being written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteTarget {
    File(String),
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NoteEditor {
    target: NoteTarget,
    text: String,
}

impl App {
    fn new(loaded: Loaded, options: Options) -> Self {
        let split = Split::new(&loaded.diff, options.layout, &HashSet::new());
        let cursor = split.first_content().unwrap_or(0);
        let key = loaded.key.clone();
        let look = Look::detect();
        let palette = Palette::of(look.colors());
        let mut app = App {
            loaded,
            palette,
            look,
            news: News::unseen(),
            clock: usage::Clock::start(),
            last_input: Instant::now(),
            polled: Instant::now(),
            stale: false,
            cursor,
            scroll: 0,
            left: 0,
            content_widths: None,
            selection: None,
            visual: false,
            focus: None,
            drag_origin: None,
            composer: None,
            rex_command: None,
            dialog: Dialog::None,
            comments: options.comments,
            marks: HashMap::new(),
            store: options.store,
            failure: options.failure,
            key,
            toast: None,
            job: None,
            copy: None,
            quit: false,
            leave: Leave::Quit,
            tabs: options.tabs,
            keys: options.keys,
            layout: options.layout,
            split,
            folded: HashSet::new(),
            viewed: HashSet::new(),
            view: Rect::default(),
            composer_view: 0,
            highlight: HashMap::new(),
            new_highlights: HIGHLIGHT_BUDGET,
            deliver: Box::new(agent::deliver),
            notes: options
                .margin
                .draft
                .as_deref()
                .map(FileNotes::from_json)
                .unwrap_or_default(),
            save_notes: options.margin.save,
            reopen_notes: options.margin.reopen,
            sends: options.margin.sends,
            overview: None,
            outline: None,
            outline_columns: outline::SIDEBAR_COLUMNS,
            note_editor: None,
            outcome: None,
            quit_armed: false,
        };
        if options.margin.overview_first && !app.loaded.diff.files().is_empty() {
            app.overview = Some(1);
        }
        app.rebuild_marks();
        app.follow_renames();
        app.refresh_viewed();
        app.fold_viewed();
        if !app.folded.is_empty() {
            app.rebuild_split();
            app.cursor = app.split.first_content().unwrap_or(0);
        }
        app
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
        loop {
            self.receive_job();
            self.poll();
            // The reply is read straight from stdin, so a key typed meanwhile
            // could be taken with it: ask only while the reader is idle (no
            // key waiting, none in the last second, not writing a comment).
            let idle = self.composer.is_none()
                && self.last_input.elapsed() >= scheme::EVERY
                && !event::poll(Duration::ZERO)?;
            if self.look.refresh(idle) {
                self.palette = Palette::of(self.look.colors());
            }
            terminal.draw(|f| self.draw(f))?;
            if let Some(text) = self.copy.take() {
                let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                let mut out = io::stdout();
                // A terminal that refuses the clipboard write only loses the copy.
                let _ = write!(out, "\x1b]52;c;{encoded}\x07").and_then(|_| out.flush());
            }
            self.expire_toast();
            if self.quit {
                return Ok(());
            }
            if let Some(wait) = self.wait_duration()
                && !event::poll(wait)?
            {
                continue;
            }
            loop {
                let event = event::read()?;
                if matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
                    self.last_input = Instant::now();
                    self.clock.touch();
                }
                match event {
                    Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
                    Event::Mouse(mouse) => self.mouse(mouse),
                    Event::Paste(text) => self.paste(&text),
                    Event::Resize(..) => {}
                    _ => {}
                }
                if self.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }

    fn expire_toast(&mut self) {
        if let Some(toast) = &self.toast
            && Instant::now() >= toast.until
        {
            self.toast = None;
        }
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::message(text));
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast::error(text));
    }

    fn can_refresh(&self) -> bool {
        self.composer.is_none()
            && self.note_editor.is_none()
            && matches!(self.dialog, Dialog::None)
            && self.selection.is_none()
            && !self.visual
            && self.drag_origin.is_none()
            && self.rex_command.is_none()
    }

    fn poll(&mut self) {
        if self.job.is_none()
            && self.can_refresh()
            && self.polled.elapsed() >= self.loaded.request.refresh_interval()
        {
            self.start_load(self.loaded.request.clone(), None);
        }
    }

    fn receive_job(&mut self) {
        if matches!(
            self.job,
            Some(Job::Load {
                background: true,
                ..
            })
        ) && !self.can_refresh()
        {
            return;
        }
        let Some(job) = self.job.take() else {
            return;
        };
        let done = match job {
            Job::Load {
                rx,
                remember,
                background,
            } => match rx.try_recv() {
                Ok(result) => Some(Done::Load {
                    result: Box::new(result),
                    remember,
                    background,
                }),
                Err(TryRecvError::Empty) => {
                    self.job = Some(Job::Load {
                        rx,
                        remember,
                        background,
                    });
                    None
                }
                Err(TryRecvError::Disconnected) => Some(Done::Load {
                    result: Box::new(Err(diff::DiffFailed::Git {
                        command: "leaf".to_owned(),
                        cause: "the loader stopped before returning a diff".to_owned(),
                    })),
                    remember,
                    background,
                }),
            },
            Job::Post {
                rx,
                index,
                composer,
                added,
            } => match rx.try_recv() {
                Ok(result) => Some(Done::Post {
                    result,
                    index,
                    composer,
                    added,
                }),
                Err(TryRecvError::Empty) => {
                    self.job = Some(Job::Post {
                        rx,
                        index,
                        composer,
                        added,
                    });
                    None
                }
                Err(TryRecvError::Disconnected) => Some(Done::Post {
                    result: Err(github::GithubFailed::Network {
                        forge: github::Forge::GitHub.title(),
                        cause: "the poster stopped before returning an answer".to_owned(),
                    }),
                    index,
                    composer,
                    added,
                }),
            },
        };
        if let Some(done) = done {
            self.finish(done);
        }
    }

    fn finish(&mut self, done: Done) {
        match done {
            Done::Load {
                result,
                remember,
                background,
            } => match *result {
                Ok(loaded) => {
                    let same_key = loaded.key == self.key;
                    if same_key && loaded.diff.same_content(&self.loaded.diff) {
                        self.loaded = loaded;
                        self.stale = false;
                        if !background {
                            self.dialog = Dialog::None;
                            self.toast("Diff unchanged");
                        }
                        return;
                    }
                    if same_key
                        && !diffreview::keeps_comments(
                            &self.loaded.diff,
                            &loaded.diff,
                            &self.comments,
                        )
                    {
                        if !self.stale || !background {
                            self.toast("Annotated files changed; keeping the reviewed snapshot until their line comments are cleared");
                        }
                        self.stale = true;
                        self.dialog = Dialog::None;
                        return;
                    }
                    self.stale = false;
                    let remember = if background {
                        self.remember()
                    } else {
                        remember
                    };
                    let previous_cursor = self.cursor;
                    let offset = previous_cursor.saturating_sub(self.scroll);
                    let picked = self
                        .overview
                        .filter(|at| *at > 0)
                        .and_then(|at| self.loaded.diff.file(at - 1))
                        .map(|file| file.path().to_owned());
                    // A folded file that changed opens again, so the change
                    // isn't hidden under its heading.
                    let changed: Vec<String> = if same_key {
                        self.folded
                            .iter()
                            .filter(|path| {
                                let print = |diff: &Diff| {
                                    diff.files()
                                        .iter()
                                        .position(|file| file.path() == path.as_str())
                                        .map(|file| diffreview::fingerprint(diff, file))
                                };
                                print(&self.loaded.diff) != print(&loaded.diff)
                            })
                            .cloned()
                            .collect()
                    } else {
                        self.folded.iter().cloned().collect()
                    };
                    for path in changed {
                        self.folded.remove(&path);
                    }
                    self.loaded = loaded;
                    self.content_widths = None;
                    self.key = self.loaded.key.clone();
                    if !same_key {
                        self.load_comments();
                        self.load_notes();
                    }
                    self.follow_renames();
                    self.refresh_viewed();
                    if !same_key {
                        self.fold_viewed();
                    }
                    self.split = Split::new(&self.loaded.diff, self.layout, &self.folded_files());
                    if let Some(open) = self.outline.as_mut() {
                        open.retarget(outline::from_diff_files(self.loaded.diff.files()), false);
                    }
                    self.cursor = if background {
                        remember
                            .and_then(|(file, side, line)| {
                                self.loaded.diff.exact_row(&file, side, line)
                            })
                            .and_then(|row| self.split.line_for_row(row))
                            .unwrap_or_else(|| self.split.snap(previous_cursor))
                    } else {
                        remember
                            .and_then(|(file, side, line)| {
                                self.loaded.diff.row_for(&file, side, line)
                            })
                            .and_then(|row| self.split.line_for_row(row))
                            .or_else(|| self.split.first_content())
                            .unwrap_or(0)
                    };
                    self.scroll = self.cursor.saturating_sub(if background {
                        offset
                    } else {
                        self.view.height as usize / 2
                    });
                    if let Some(picked) = picked {
                        self.overview = Some(
                            self.loaded
                                .diff
                                .files()
                                .iter()
                                .position(|file| file.path() == picked)
                                .map_or(1.min(self.loaded.diff.files().len()), |at| at + 1),
                        );
                    }
                    self.rebuild_marks();
                    self.selection = None;
                    self.focus = None;
                    self.composer = None;
                    if !background {
                        self.left = 0;
                    }
                    self.highlight.clear();
                    self.dialog = Dialog::None;
                    let _ = execute!(io::stdout(), SetTitle(&self.loaded.title));
                    if !background {
                        self.toast(format!("Loaded {}", self.loaded.label));
                    }
                }
                Err(e) => {
                    if !background {
                        self.dialog = Dialog::Error(e.to_string());
                    }
                }
            },
            Done::Post {
                result,
                index,
                composer,
                added,
            } => match result {
                Ok(pull) => {
                    self.dialog = Dialog::None;
                    self.set_state(index, State::Github);
                    self.toast(format!("Comment posted to {pull}"));
                }
                Err(e) => {
                    if added {
                        self.remove(index);
                    }
                    self.composer = Some(composer);
                    self.dialog = Dialog::Error(e.to_string());
                }
            },
        }
    }

    fn load_comments(&mut self) {
        let loaded = self
            .store
            .as_ref()
            .map(|store| store.load_diff_comments(&self.key));
        self.comments = match loaded {
            Some(Ok(comments)) => comments,
            Some(Err(e)) => {
                self.store_failure(e);
                Vec::new()
            }
            None => Vec::new(),
        };
        self.rebuild_marks();
    }

    /// The file and global notes kept for the current key, which also become
    /// where later changes are saved.
    fn load_notes(&mut self) {
        let Some(reopen) = self.reopen_notes.as_mut() else {
            return;
        };
        let (draft, save) = reopen(&self.key);
        self.notes = draft
            .as_deref()
            .map(FileNotes::from_json)
            .unwrap_or_default();
        self.save_notes = save;
        self.note_editor = None;
    }

    fn store_failure(&mut self, error: StoreFailed) {
        *self.failure.borrow_mut() = Some(error.to_string());
        self.toast = Some(Toast::error(format!(
            "This comment stays in memory only: {error}"
        )));
    }

    fn rebuild_marks(&mut self) {
        self.marks.clear();
        for saved in &self.comments {
            let anchor = &saved.comment.anchor;
            let mut rows = Vec::new();
            if let Some(row) =
                self.loaded
                    .diff
                    .row_for(&anchor.file, anchor.side, anchor.start_line)
            {
                rows.push(row);
            }
            if anchor.end_line != anchor.start_line
                && let Some(row) =
                    self.loaded
                        .diff
                        .row_for(&anchor.file, anchor.side, anchor.end_line)
            {
                rows.push(row);
            }
            for row in rows {
                self.marks.insert(row, saved.comment.state);
            }
        }
    }

    fn add_comment(&mut self, comment: Comment) -> usize {
        let saved = self
            .store
            .as_ref()
            .map(|store| store.insert_diff_comment(&self.key, &comment));
        let id = match saved {
            Some(Ok(id)) => id,
            Some(Err(e)) => {
                self.store_failure(e);
                -1
            }
            None => -1,
        };
        self.comments.push(SavedComment { id, comment });
        let index = self.comments.len() - 1;
        self.rebuild_marks();
        index
    }

    fn set_state(&mut self, index: usize, state: State) {
        let Some(saved) = self.comments.get_mut(index) else {
            return;
        };
        saved.comment.state = state;
        let id = saved.id;
        if id >= 0 {
            let result = self
                .store
                .as_ref()
                .map(|store| store.set_diff_comment_state(id, state));
            if let Some(Err(e)) = result {
                self.store_failure(e);
            }
        }
        self.rebuild_marks();
    }
}

// PART2

/// Opens the reviewer on a loaded patch, and says how it was left.
pub fn run(loaded: Loaded, options: Options) -> io::Result<Leave> {
    open(loaded, options).map(|app| app.leave)
}

/// Opens the reviewer for a Margin round: on the overview of every file, with
/// `S` closing it and handing the review back.
pub fn review(loaded: Loaded, mut options: Options) -> io::Result<Outcome> {
    options.margin.sends = Sends::Return;
    options.margin.overview_first = true;
    open(loaded, options).map(|app| app.outcome.unwrap_or(Outcome::Closed))
}

fn open(loaded: Loaded, options: Options) -> io::Result<App> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    let setup = execute!(
        io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste,
        SetTitle(&loaded.title)
    );
    // Lets Rex's command palette find this diff; Rex drops it if leaf dies.
    remote::announce(remote::Kind::Diff, &loaded.title);
    let mut app = App::new(loaded, options);
    let result = setup.and_then(|_| app.event_loop(&mut terminal));
    remote::withdraw(remote::Kind::Diff);
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result.map(|()| app)
}

/// The text sent to an agent or posted as a comment: the file and line range,
/// the hunk around that range, then the comment.
fn payload(diff: &Diff, anchor: &Anchor, first: usize, last: usize, body: &str) -> String {
    let side = match anchor.side {
        Side::New => "new side",
        Side::Old => "old side",
    };
    let mut out = format!(
        "> {} {} ({side}). The diff includes the lines around that range:\n\n```diff\n{}\n```",
        anchor.file,
        anchor.range_label(),
        diff.neighborhood(first, last, 8)
    );
    let body = body.trim();
    if !body.is_empty() {
        out.push_str("\n\n");
        out.push_str(body);
    }
    out.push('\n');
    out
}

enum Edit {
    None,
    Save,
    Cancel,
    Agent,
    Github,
}

impl Composer {
    fn new(anchor: Anchor, first: usize, last: usize, purpose: Purpose) -> Self {
        Composer {
            text: Vec::new(),
            cursor: 0,
            anchor,
            first,
            last,
            purpose,
        }
    }

    fn from_comment(comment: &Comment, first: usize, last: usize) -> Self {
        let text: Vec<char> = comment.body.chars().collect();
        Composer {
            cursor: text.len(),
            text,
            anchor: comment.anchor.clone(),
            first,
            last,
            purpose: Purpose::Comment,
        }
    }

    fn text(&self) -> String {
        self.text.iter().collect()
    }

    fn body(&self) -> String {
        self.text().trim().to_owned()
    }

    fn insert(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\r' {
                continue;
            }
            self.text.insert(self.cursor, ch);
            self.cursor += 1;
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.len());
    }

    fn home(&mut self) {
        self.cursor = self
            .text
            .iter()
            .take(self.cursor)
            .rposition(|ch| *ch == '\n')
            .map_or(0, |at| at + 1);
    }

    fn end(&mut self) {
        self.cursor = self
            .text
            .iter()
            .skip(self.cursor)
            .position(|ch| *ch == '\n')
            .map_or(self.text.len(), |at| self.cursor + at);
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.text.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    fn delete_word(&mut self) {
        let mut at = self.cursor;
        while at > 0 && self.text[at - 1].is_whitespace() {
            at -= 1;
        }
        while at > 0 && !self.text[at - 1].is_whitespace() {
            at -= 1;
        }
        self.text.drain(at..self.cursor);
        self.cursor = at;
    }

    /// The text split into visual lines of at most `width` columns, and the
    /// visual line holding the cursor.
    fn wrap(&self, width: usize) -> (Vec<(usize, usize)>, usize) {
        let width = width.max(1);
        let mut lines = Vec::new();
        let mut start = 0;
        let mut column = 0;
        for (at, ch) in self.text.iter().enumerate() {
            if *ch == '\n' {
                lines.push((start, at));
                start = at + 1;
                column = 0;
                continue;
            }
            let cells = ch.width().unwrap_or(0);
            if column + cells > width && at > start {
                lines.push((start, at));
                start = at;
                column = 0;
            }
            column += cells;
        }
        lines.push((start, self.text.len()));
        let cursor = lines
            .iter()
            .rposition(|(start, _)| *start <= self.cursor)
            .unwrap_or(0);
        (lines, cursor)
    }

    fn edit(&mut self, key: KeyEvent) -> Edit {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Enter => Edit::Agent,
                KeyCode::Char('g') => Edit::Github,
                KeyCode::Char('j') => {
                    self.insert("\n");
                    Edit::None
                }
                KeyCode::Char('w') => {
                    self.delete_word();
                    Edit::None
                }
                KeyCode::Char('u') => {
                    self.text.drain(..self.cursor);
                    self.cursor = 0;
                    Edit::None
                }
                _ => Edit::None,
            };
        }
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                self.insert("\n");
                Edit::None
            }
            KeyCode::Enter => Edit::Save,
            KeyCode::Esc => Edit::Cancel,
            KeyCode::Backspace => {
                self.backspace();
                Edit::None
            }
            KeyCode::Delete => {
                self.delete();
                Edit::None
            }
            KeyCode::Left => {
                self.left();
                Edit::None
            }
            KeyCode::Right => {
                self.right();
                Edit::None
            }
            KeyCode::Home => {
                self.home();
                Edit::None
            }
            KeyCode::End => {
                self.end();
                Edit::None
            }
            KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::SUPER) => {
                self.insert(&ch.to_string());
                Edit::None
            }
            KeyCode::Tab => {
                self.insert("  ");
                Edit::None
            }
            _ => Edit::None,
        }
    }
}

impl App {
    fn dialog_kind(&self) -> Option<DialogKind> {
        match &self.dialog {
            Dialog::None => None,
            dialog => Some(dialog.kind()),
        }
    }

    fn key(&mut self, key: KeyEvent) {
        // Before dialogs and the comment box, which would take it as typing.
        if key.code == remote::WAKE {
            self.rex_command = Some(String::new());
            return;
        }
        if let Some(mut line) = self.rex_command.take() {
            match key.code {
                KeyCode::Enter => self.obey_rex(remote::Command::parse(&line)),
                KeyCode::Esc => {}
                KeyCode::Char(c) if line.len() < remote::LONGEST => {
                    line.push(c);
                    self.rex_command = Some(line);
                }
                _ => self.rex_command = Some(line),
            }
            return;
        }
        if News::key(&mut self.news, key) {
            return;
        }
        if self.dialog_kind().is_some() {
            self.dialog_key(key);
            return;
        }
        if self.note_editor.is_some() {
            self.editor_key(key);
            return;
        }
        if self.composer.is_some() {
            self.composer_key(key);
            return;
        }
        if self.overview.is_some() {
            self.overview_key(key);
            return;
        }
        if self.outline.is_some() && self.outline_key(key) {
            return;
        }
        let actions = self.keys.actions(&key);
        // `q` quits on the second press; any other key between starts over.
        if !actions.contains(&DiffAction::Quit) {
            self.quit_armed = false;
        }
        if actions.into_iter().any(|action| self.perform(action)) {
            return;
        }
        if key.code == KeyCode::Esc {
            // Escape drops whatever is highlighted before it quits.
            if self.visual {
                self.visual = false;
                self.selection = None;
            } else if self.selection.is_some() || self.focus.is_some() {
                self.selection = None;
                self.focus = None;
            } else {
                self.quit = true;
            }
        }
    }

    /// Does what `action` asks, and says whether it did anything. One that
    /// can't act now lets the next action bound to the same key have it.
    fn perform(&mut self, action: DiffAction) -> bool {
        let tab = |leave| (self.tabs).then_some(leave);
        let leave = match action {
            DiffAction::TabSummary => tab(Leave::Summary),
            DiffAction::TabTimeline => tab(Leave::Timeline),
            DiffAction::TabComments => tab(Leave::Comments),
            DiffAction::TabChecks => tab(Leave::Checks),
            _ => None,
        };
        if let Some(leave) = leave {
            self.leave = leave;
            self.quit = true;
            return true;
        }
        match action {
            DiffAction::TabSummary
            | DiffAction::TabTimeline
            | DiffAction::TabComments
            | DiffAction::TabChecks => return false,
            DiffAction::Quit => self.ask_quit(),
            DiffAction::QuitNow => self.quit = true,
            DiffAction::FileNote => self.edit_file_note(None),
            DiffAction::GlobalNote => self.edit_global_note(),
            DiffAction::Viewed => self.toggle_viewed(None),
            DiffAction::SendReview => self.open_verdict(),
            DiffAction::Overview => self.show_overview(),
            DiffAction::Down => self.move_cursor(true),
            DiffAction::Up => self.move_cursor(false),
            DiffAction::HalfPageDown => self.page(true, true),
            DiffAction::HalfPageUp => self.page(false, true),
            DiffAction::PageDown => self.page(true, false),
            DiffAction::PageUp => self.page(false, false),
            DiffAction::First => self.go_edge(false),
            DiffAction::Last => self.go_edge(true),
            DiffAction::NextFile => self.next_file(true),
            DiffAction::PreviousFile => self.next_file(false),
            DiffAction::Files => self.show_files(),
            DiffAction::Outline => self.toggle_outline(OutlineForm::Sidebar),
            DiffAction::OutlineWindow => self.toggle_outline(OutlineForm::Window),
            DiffAction::Select => self.toggle_selection(),
            DiffAction::Comment => self.compose(Purpose::Comment),
            DiffAction::Ask => self.compose(Purpose::Ask),
            DiffAction::Comments => self.show_comments(),
            DiffAction::Copy => self.copy_payload(false),
            DiffAction::CopyAll => self.copy_payload(true),
            DiffAction::OtherSide => self.other_side(),
            DiffAction::Layout => self.toggle_layout(),
            DiffAction::Reload => self.reload(),
            DiffAction::Fold => self.fold_current(true),
            DiffAction::Unfold => self.fold_current(false),
            DiffAction::ScrollLeft => self.scroll_sideways(false),
            DiffAction::ScrollRight => self.scroll_sideways(true),
            DiffAction::ScrollReset => self.left = 0,
            DiffAction::Help => self.dialog = Dialog::Help,
        }
        true
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let (action, purpose) = match &mut self.composer {
            Some(composer) => (composer.edit(key), composer.purpose),
            None => (Edit::None, Purpose::Comment),
        };
        match action {
            Edit::None => {}
            Edit::Save if purpose == Purpose::Ask => self.send_to_agent(),
            Edit::Save => self.save_comment(),
            Edit::Cancel => self.cancel_composer(),
            Edit::Agent => self.send_to_agent(),
            Edit::Github => self.ask_github(),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent) {
        match self.dialog_kind() {
            Some(DialogKind::Comments) => self.comments_key(key),
            Some(DialogKind::Files) => self.files_key(key),
            Some(DialogKind::Confirm) => self.confirm_key(key),
            // Every key is the window's.
            Some(DialogKind::Outline) => {
                self.outline_key(key);
            }
            Some(DialogKind::Loading) => {}
            Some(DialogKind::Verdict) => self.verdict_key(key),
            Some(DialogKind::Help | DialogKind::Error) | None => {
                let closes =
                    self.keys.is(&key, DiffAction::Quit) || self.keys.is(&key, DiffAction::Help);
                if closes || matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.dialog = Dialog::None;
                }
            }
        }
    }

    fn selected_comment(&self) -> Option<(usize, Comment)> {
        match &self.dialog {
            Dialog::Comments { selected } => self
                .comments
                .get(*selected)
                .map(|saved| (*selected, saved.comment.clone())),
            _ => None,
        }
    }

    fn comments_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('m') => self.dialog = Dialog::None,
            KeyCode::Down | KeyCode::Char('j') => {
                let last = self.comments.len().saturating_sub(1);
                if let Dialog::Comments { selected } = &mut self.dialog {
                    *selected = (*selected + 1).min(last);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Dialog::Comments { selected } = &mut self.dialog {
                    *selected = selected.saturating_sub(1);
                }
            }
            KeyCode::Enter => {
                if let Some((_, comment)) = self.selected_comment() {
                    let anchor = comment.anchor;
                    self.dialog = Dialog::None;
                    self.jump_to(&anchor);
                }
            }
            KeyCode::Char('d') => {
                if let Some((index, _)) = self.selected_comment() {
                    self.remove(index);
                    self.toast("Comment deleted");
                    if self.comments.is_empty() {
                        self.dialog = Dialog::None;
                    } else if let Dialog::Comments { selected } = &mut self.dialog {
                        *selected = (*selected).min(self.comments.len() - 1);
                    }
                }
            }
            KeyCode::Char('g') => {
                if let Some((index, comment)) = self.selected_comment() {
                    self.dialog = Dialog::None;
                    self.comment_to_github(&comment, Some(index));
                }
            }
            KeyCode::Char('a') => {
                if let Some((index, comment)) = self.selected_comment() {
                    self.dialog = Dialog::None;
                    self.comment_to_agent(&comment, Some(index));
                }
            }
            KeyCode::Char('y') => {
                if let Some((_, comment)) = self.selected_comment() {
                    let text = self.anchor_payload(&comment.anchor, &comment.body);
                    self.copy = Some(text);
                    self.toast("Comment copied");
                }
            }
            _ => {}
        }
    }

    fn confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.start_post(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                if let Dialog::Confirm(confirm) = std::mem::replace(&mut self.dialog, Dialog::None)
                {
                    self.composer = Some(confirm.composer);
                }
            }
            _ => {}
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        if News::mouse(&mut self.news, mouse)
            || self.dialog_kind().is_some()
            || self.composer.is_some()
        {
            return;
        }
        let beside = match self.form() {
            Some(OutlineForm::Sidebar) => self.view.x,
            _ => 0,
        };
        if let Some(open) = self.outline.as_mut() {
            let (taken, width) = open.edge_mouse(mouse.kind, mouse.column, beside);
            if let Some(width) = width {
                self.outline_columns = width;
            }
            if taken {
                return;
            }
        }
        match self.form() {
            // A release is let through, so a drag begun in the diff still ends.
            Some(OutlineForm::Sidebar)
                if mouse.column < self.view.x
                    && !matches!(
                        mouse.kind,
                        MouseEventKind::ScrollDown
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::Up(_)
                    ) =>
            {
                return;
            }
            _ => {}
        }
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight
        ) {
            if self.overview.is_none() && self.note_editor.is_none() && self.rex_command.is_none() {
                self.scroll_sideways(mouse.kind == MouseEventKind::ScrollRight);
            }
            return;
        }
        let (top, height, scroll) = (self.view.y, self.view.height, self.scroll);
        let pinned = self.pinned_header();
        let offset = u16::from(pinned.is_some());
        let row_at = |y: u16| -> Option<usize> {
            let inside = y.checked_sub(top)?;
            if inside >= height {
                return None;
            }
            if inside < offset {
                return pinned;
            }
            Some(scroll + (inside - offset) as usize)
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.clear_selection_on_move();
                let before = self.scroll;
                self.scroll = (self.scroll + WHEEL_ROWS).min(self.max_scroll());
                let moved = self.scroll - before;
                self.cursor = self.snap(self.cursor + moved);
            }
            MouseEventKind::ScrollUp => {
                self.clear_selection_on_move();
                let before = self.scroll;
                self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
                let moved = before - self.scroll;
                self.cursor = self.snap(self.cursor.saturating_sub(moved));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = row_at(mouse.row)
                    && row < self.split.lines().len()
                {
                    let row = self.snap(row);
                    self.visual = false;
                    self.focus = None;
                    self.cursor = row;
                    self.selection = None;
                    self.drag_origin = Some(row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(origin) = self.drag_origin
                    && let Some(row) = row_at(mouse.row)
                    && row < self.split.lines().len()
                {
                    self.selection = Some(origin);
                    self.cursor = self.snap(row);
                    self.drag_edge(mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag_origin = None,
            _ => {}
        }
    }

    fn max_left(&mut self) -> usize {
        let diff = &self.loaded.diff;
        let (old, new) = *self.content_widths.get_or_insert_with(|| {
            let (mut old, mut new) = (0, 0);
            for (i, row) in diff
                .rows()
                .iter()
                .enumerate()
                .filter(|(_, row)| row.kind.is_content())
            {
                let width = cells(diff.content(i));
                if row.kind != RowKind::Added {
                    old = old.max(width);
                }
                if row.kind != RowKind::Removed {
                    new = new.max(width);
                }
            }
            (old, new)
        });
        let (left, right) = self.layout.pane_widths(self.view.width);
        let gutter = self.layout.gutter(self.number_width());
        let overflow = |width: usize, pane: u16| match (pane as usize).saturating_sub(gutter) {
            0 => 0,
            room => width.saturating_sub(room),
        };
        overflow(old, left).max(overflow(new, right))
    }

    fn scroll_sideways(&mut self, right: bool) {
        self.left = if right {
            self.left.saturating_add(4).min(self.max_left())
        } else {
            self.left.saturating_sub(4)
        };
    }

    /// Scrolls when a drag reaches past the top or bottom of the view.
    fn drag_edge(&mut self, y: u16) {
        let bottom = self.view.y + self.view.height.saturating_sub(1);
        if y <= self.view.y {
            self.scroll = self.scroll.saturating_sub(1);
        } else if y >= bottom {
            self.scroll = (self.scroll + 1).min(self.max_scroll());
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(editor) = &mut self.note_editor {
            for ch in text.chars().filter(|ch| *ch != '\r') {
                editor.text.push(ch);
            }
            return;
        }
        if let Some(composer) = &mut self.composer {
            composer.insert(text);
        }
    }

    fn max_scroll(&self) -> usize {
        self.split
            .lines()
            .len()
            .saturating_sub(self.view.height as usize)
    }

    /// The diff row the cursor is anchored to.
    fn current_row(&self) -> Option<usize> {
        self.split
            .lines()
            .get(self.cursor)
            .and_then(|line| line.focus_row())
    }

    fn current_file(&self) -> Option<usize> {
        self.current_row()
            .and_then(|row| self.loaded.diff.file_index(row))
    }

    fn snap(&self, line: usize) -> usize {
        self.split.snap(line)
    }

    /// A retained selection ends when the reader moves on; in visual mode the
    /// selection stays and the movement extends it.
    fn clear_selection_on_move(&mut self) {
        self.focus = None;
        if !self.visual {
            self.selection = None;
        }
    }

    fn move_cursor(&mut self, forward: bool) {
        self.clear_selection_on_move();
        if let Some(next) = self.split.next_content(self.cursor, forward) {
            self.cursor = next;
        }
        self.follow_cursor();
    }

    fn page(&mut self, forward: bool, half: bool) {
        self.clear_selection_on_move();
        let step = (self.view.height as usize).max(1);
        let step = if half {
            (step / 2).max(1)
        } else {
            step.saturating_sub(1).max(1)
        };
        let last = self.split.lines().len().saturating_sub(1);
        let target = if forward {
            (self.cursor + step).min(last)
        } else {
            self.cursor.saturating_sub(step)
        };
        self.cursor = self.snap(target);
        self.follow_cursor();
    }

    fn go_edge(&mut self, end: bool) {
        self.clear_selection_on_move();
        self.cursor = if end {
            self.split.last_content()
        } else {
            self.split.first_content()
        }
        .unwrap_or(0);
        self.follow_cursor();
    }

    fn next_file(&mut self, forward: bool) {
        self.clear_selection_on_move();
        let diff = &self.loaded.diff;
        if diff.files().is_empty() {
            return;
        }
        let current = self.current_file().unwrap_or(0);
        // Reviewing, the files come round through the overview.
        let last = diff.files().len() - 1;
        if self.sends == Sends::Return
            && ((forward && current == last) || (!forward && current == 0))
        {
            return self.show_overview();
        }
        let index = if forward {
            (current + 1) % diff.files().len()
        } else {
            (current + diff.files().len() - 1) % diff.files().len()
        };
        let count = diff.files().len();
        self.jump_file(index);
        self.toast(format!("{} of {count}", index + 1));
    }

    fn jump_file(&mut self, index: usize) {
        let line = self
            .loaded
            .diff
            .first_content(index)
            .and_then(|row| self.split.line_for_row(row));
        if let Some(line) = line {
            self.visual = false;
            self.selection = None;
            self.focus = None;
            self.cursor = line;
            self.follow_cursor();
        }
    }

    /// Jumps to file `index`, opening it when it's folded.
    fn open_file(&mut self, index: usize) {
        self.set_folded(index, false);
        self.jump_file(index);
    }

    fn show_files(&mut self) {
        if self.loaded.diff.files().is_empty() {
            self.toast("This diff has no files");
            return;
        }
        let selected = self.current_file().unwrap_or(0);
        self.dialog = Dialog::Files {
            selected,
            query: String::new(),
        };
    }

    /// The files the picker lists, in diff order, filtered by the search bar.
    fn picker_files(&self) -> Vec<usize> {
        match &self.dialog {
            Dialog::Files { query, .. } => filtered_files(&self.loaded.diff, query),
            _ => Vec::new(),
        }
    }

    fn files_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            // A search is the closest thing to dismiss, so Escape empties it first.
            KeyCode::Esc => {
                if self.files_searching() {
                    let keep = self
                        .picker_files()
                        .get(self.pick_selected())
                        .copied()
                        .unwrap_or(0);
                    if let Dialog::Files { selected, query } = &mut self.dialog {
                        query.clear();
                        *selected = keep;
                    }
                } else {
                    self.dialog = Dialog::None;
                }
            }
            KeyCode::Down => self.pick_move(1),
            KeyCode::Up => self.pick_move(-1),
            KeyCode::Char('n') if control => self.pick_move(1),
            KeyCode::Char('p') if control => self.pick_move(-1),
            KeyCode::Home => self.pick_to(0),
            KeyCode::End => self.pick_to(usize::MAX),
            KeyCode::Backspace => {
                if let Dialog::Files { selected, query } = &mut self.dialog {
                    query.pop();
                    *selected = 0;
                }
            }
            KeyCode::Char('u') if control => {
                if let Dialog::Files { selected, query } = &mut self.dialog {
                    query.clear();
                    *selected = 0;
                }
            }
            KeyCode::Enter => {
                let files = self.picker_files();
                if let Some(&index) = files.get(self.pick_selected()) {
                    self.dialog = Dialog::None;
                    self.open_file(index);
                }
            }
            KeyCode::Char(c) if !control => {
                if let Dialog::Files { selected, query } = &mut self.dialog {
                    query.push(c);
                    *selected = 0;
                }
            }
            _ => {}
        }
    }

    fn pick_selected(&self) -> usize {
        match &self.dialog {
            Dialog::Files { selected, .. } => *selected,
            _ => 0,
        }
    }

    /// Whether the file picker has a search worth clearing.
    fn files_searching(&self) -> bool {
        match &self.dialog {
            Dialog::Files { query, .. } => !query.is_empty(),
            _ => false,
        }
    }

    fn pick_move(&mut self, delta: isize) {
        let last = self.picker_files().len().saturating_sub(1);
        let selected = self.pick_selected();
        let next = if delta < 0 {
            selected.saturating_sub(delta.unsigned_abs())
        } else {
            (selected + delta as usize).min(last)
        };
        self.pick_to(next);
    }

    fn pick_to(&mut self, index: usize) {
        let last = self.picker_files().len().saturating_sub(1);
        if let Dialog::Files { selected, .. } = &mut self.dialog {
            *selected = index.min(last);
        }
    }

    fn follow_cursor(&mut self) {
        let height = self.view.height as usize;
        if height == 0 {
            return;
        }
        if self.cursor <= self.scroll {
            // Scrolling back up keeps the file and hunk headings directly
            // above the cursor on screen, so a file's name does not disappear.
            self.scroll = self.heading_start(self.cursor);
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// The file header pinned above the lines that are on screen, when the
    /// file it belongs to is scrolled under its own name.
    fn pinned_header(&self) -> Option<usize> {
        if self.scroll == 0 {
            return None;
        }
        let diff = &self.loaded.diff;
        let lines = self.split.lines();
        let last = self.scroll.min(lines.len().saturating_sub(1));
        (0..=last)
            .rev()
            .find(|&index| {
                lines
                    .get(index)
                    .and_then(|line| line.band)
                    .is_some_and(|row| diff.rows()[row].kind == RowKind::File)
            })
            .filter(|&index| index < self.scroll)
    }

    /// The line a scroll can stop at when bringing `line` to the top: the
    /// line itself, plus the file and hunk headings directly above it.
    fn heading_start(&self, line: usize) -> usize {
        let diff = &self.loaded.diff;
        let lines = self.split.lines();
        // A file's own name is as far up as its headings go: above it is another file.
        let is_file = |at: usize| {
            lines
                .get(at)
                .and_then(|line| line.band)
                .is_some_and(|row| diff.rows()[row].kind == RowKind::File)
        };
        let mut top = line;
        while top > 0
            && !is_file(top)
            && self
                .split
                .lines()
                .get(top - 1)
                .is_some_and(|line| line.band.is_some() && !line.folded)
        {
            top -= 1;
        }
        top
    }

    /// Keeps a whole highlighted range on screen, not only its cursor.
    fn follow_focus(&mut self, first: usize, last: usize) {
        let height = self.view.height as usize;
        if height == 0 {
            return;
        }
        if last >= first + height || first < self.scroll {
            self.scroll = first;
        } else if last >= self.scroll + height {
            self.scroll = last + 1 - height;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Switches between split and inline, keeping the cursor, selection and
    /// focused comment on the same diff rows.
    fn toggle_layout(&mut self) {
        self.layout = self.layout.toggled();
        // Remembered, so the next diff opens the same way.
        match self
            .store
            .as_ref()
            .map(|store| store.save_diff_layout(self.layout))
        {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                *self.failure.borrow_mut() = Some(e.to_string());
                self.toast = Some(Toast::error(format!(
                    "This layout lasts only until you quit: {e}"
                )));
            }
            None => {
                self.toast = Some(Toast::error(
                    "This layout lasts only until you quit: leaf's settings store couldn't be opened.",
                ));
            }
        }
        self.rebuild_split();
    }

    /// The indices of the folded files in this diff.
    fn folded_files(&self) -> HashSet<usize> {
        if self.folded.is_empty() {
            return HashSet::new();
        }
        self.loaded
            .diff
            .files()
            .iter()
            .enumerate()
            .filter(|(_, file)| self.folded.contains(file.path()))
            .map(|(index, _)| index)
            .collect()
    }

    /// Works out again which files are marked viewed.
    fn refresh_viewed(&mut self) {
        let diff = &self.loaded.diff;
        self.viewed = (0..diff.files().len())
            .filter(|&index| self.notes.is_viewed(diff, index))
            .collect();
    }

    /// Folds every file marked viewed.
    fn fold_viewed(&mut self) {
        let files = self.loaded.diff.files();
        for &index in &self.viewed {
            self.folded.insert(files[index].path().to_owned());
        }
    }

    /// Lays the lines out again after a layout switch or a fold, keeping the
    /// cursor, selection and focused comment on the same diff rows.
    fn rebuild_split(&mut self) {
        let row_of =
            |split: &Split, line: usize| split.lines().get(line).and_then(|l| l.focus_row());
        let cursor = row_of(&self.split, self.cursor);
        let selection = self.selection.and_then(|line| row_of(&self.split, line));
        let focus = self
            .focus
            .and_then(|(first, last)| self.split.rows_between(first, last));
        self.split = Split::new(&self.loaded.diff, self.layout, &self.folded_files());
        self.cursor = cursor
            .and_then(|row| self.split.line_for_row(row))
            .map(|line| self.split.snap(line))
            .or_else(|| self.split.first_content())
            .unwrap_or(0);
        self.selection = selection.and_then(|row| self.split.line_for_row(row));
        self.focus = focus.and_then(|(first, last)| self.split.lines_for_rows(first, last));
        match self.focus {
            Some((first, last)) => self.follow_focus(first, last),
            None => self.follow_cursor(),
        }
    }

    /// Folds file `file` to its heading, or opens it, leaving the cursor on
    /// its heading or its first line. Returns whether anything changed.
    fn set_folded(&mut self, file: usize, fold: bool) -> bool {
        let Some(info) = self.loaded.diff.file(file) else {
            return false;
        };
        let path = info.path().to_owned();
        let header = info.header_row;
        let changed = if fold {
            self.folded.insert(path)
        } else {
            self.folded.remove(&path)
        };
        if !changed {
            return false;
        }
        self.visual = false;
        self.selection = None;
        self.focus = None;
        self.rebuild_split();
        if let Some(line) = self.split.line_for_row(header) {
            self.cursor = self.split.snap(line);
            // Keep the heading on screen.
            self.scroll = self.scroll.min(line);
        }
        self.follow_cursor();
        true
    }

    /// `h` folds the file under the cursor, `l` opens it.
    fn fold_current(&mut self, fold: bool) {
        if let Some(file) = self.current_file() {
            self.set_folded(file, fold);
        }
    }

    fn toggle_selection(&mut self) {
        self.focus = None;
        if self.visual {
            self.visual = false;
            self.selection = None;
        } else {
            self.visual = true;
            self.selection = Some(self.cursor);
        }
    }

    fn range(&self) -> (usize, usize) {
        let anchor = self.selection.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }

    /// Carries out what Rex's command palette asked for.
    fn obey_rex(&mut self, command: Result<remote::Command, String>) {
        match command {
            Ok(remote::Command::Ask(question)) => {
                self.dialog = Dialog::None;
                if self.composer.is_none() {
                    self.compose(Purpose::Ask);
                }
                if !question.trim().is_empty()
                    && let Some(composer) = &mut self.composer
                {
                    composer.purpose = Purpose::Ask;
                    composer.insert(&question);
                    self.send_to_agent();
                }
            }
            Ok(remote::Command::Quit) => self.quit = true,
            Ok(_) => self.toast("Reading aloud and voices work in documents, not diffs."),
            Err(message) => self.toast(message),
        }
    }

    fn compose(&mut self, purpose: Purpose) {
        let (start, end) = self.range();
        let rows = self.split.comment_rows(start, end);
        let range = rows.and_then(|(first, last)| self.loaded.diff.comment_range(first, last));
        match range {
            Some((first, last, anchor)) => {
                let lines = self.split.lines_for_rows(first, last);
                self.visual = false;
                self.focus = None;
                if let Some((first_line, last_line)) = lines {
                    self.cursor = last_line;
                    self.selection = Some(first_line);
                    self.follow_focus(first_line, last_line);
                }
                self.composer = Some(Composer::new(anchor, first, last, purpose));
            }
            None => self.fail(
                "That range has no code line to comment on. Move to a line that changed, then press C.",
            ),
        }
    }

    fn send_to_agent(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        if body.is_empty() {
            self.fail("The comment is empty. Write something, or press Esc to discard it.");
            self.composer = Some(composer);
            return;
        }
        let text = payload(
            &self.loaded.diff,
            &composer.anchor,
            composer.first,
            composer.last,
            &body,
        );
        let comment = Comment {
            anchor: composer.anchor.clone(),
            body,
            state: State::Agent,
        };
        match (self.deliver)(&text) {
            Ok(agent) => {
                self.add_comment(comment);
                self.selection = self.split.line_for_row(composer.first);
                self.toast(format!("Sent to {}", agent.describe()));
            }
            Err(e) => {
                self.add_comment(Comment {
                    state: State::Draft,
                    ..comment
                });
                self.copy = Some(text);
                self.fail(e.to_string());
            }
        }
    }

    fn save_comment(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        if body.is_empty() {
            self.composer = Some(composer);
            self.toast("Write something first, or press Esc to cancel");
            return;
        }
        self.add_comment(Comment {
            anchor: composer.anchor,
            body,
            state: State::Draft,
        });
        self.toast("Comment saved: m lists comments");
    }

    fn cancel_composer(&mut self) {
        if self.composer.take().is_some() {
            self.toast("Comment discarded");
        }
    }

    fn ask_github(&mut self) {
        if self.stale {
            self.fail("The reviewed snapshot is out of date; clear its line comments and refresh before posting to GitHub");
            return;
        }
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        if body.is_empty() {
            self.fail("The comment is empty, so there is nothing to post.");
            self.composer = Some(composer);
            return;
        }
        self.dialog = Dialog::Confirm(Confirm {
            body,
            anchor: composer.anchor.clone(),
            composer,
            edit: None,
        });
    }

    fn comment_to_github(&mut self, comment: &Comment, index: Option<usize>) {
        if self.stale {
            self.fail("The reviewed snapshot is out of date; clear its line comments and refresh before posting to GitHub");
            return;
        }
        let (Some(first), Some(last)) = (
            self.loaded.diff.row_for(
                &comment.anchor.file,
                comment.anchor.side,
                comment.anchor.start_line,
            ),
            self.loaded.diff.row_for(
                &comment.anchor.file,
                comment.anchor.side,
                comment.anchor.end_line,
            ),
        ) else {
            self.fail("Those lines are no longer in the diff. Reload with r, then try again.");
            return;
        };
        self.dialog = Dialog::Confirm(Confirm {
            body: comment.body.clone(),
            anchor: comment.anchor.clone(),
            composer: Composer::from_comment(comment, first, last),
            edit: index,
        });
    }

    fn comment_to_agent(&mut self, comment: &Comment, index: Option<usize>) {
        let text = self.anchor_payload(&comment.anchor, &comment.body);
        match (self.deliver)(&text) {
            Ok(agent) => {
                if let Some(index) = index {
                    self.set_state(index, State::Agent);
                }
                self.toast(format!("Sent to {}", agent.describe()));
            }
            Err(e) => {
                self.copy = Some(text);
                self.fail(e.to_string());
            }
        }
    }

    /// A saved comment's text, found by its anchor rather than a row selection.
    fn anchor_payload(&self, anchor: &Anchor, body: &str) -> String {
        let first = self
            .loaded
            .diff
            .row_for(&anchor.file, anchor.side, anchor.start_line)
            .unwrap_or(0);
        let last = self
            .loaded
            .diff
            .row_for(&anchor.file, anchor.side, anchor.end_line)
            .unwrap_or(first);
        payload(&self.loaded.diff, anchor, first, last, body)
    }

    fn start_post(&mut self) {
        let Dialog::Confirm(confirm) = std::mem::replace(
            &mut self.dialog,
            Dialog::Loading("Posting to GitHub…".to_owned()),
        ) else {
            return;
        };
        let (index, added) = match confirm.edit {
            Some(index) => (index, false),
            None => {
                let index = self.add_comment(Comment {
                    anchor: confirm.anchor.clone(),
                    body: confirm.body.clone(),
                    state: State::Draft,
                });
                (index, true)
            }
        };
        let (tx, rx) = mpsc::channel();
        let resource = self.loaded.github.clone();
        let repo = self.loaded.request.repo.clone();
        let branch = self.loaded.branch.clone();
        let anchor = confirm.anchor.clone();
        let body = confirm.body.clone();
        std::thread::spawn(move || {
            let result = github::post_review(
                resource.as_ref(),
                repo.as_ref(),
                branch.as_deref(),
                &anchor,
                &body,
            );
            let _ = tx.send(result);
        });
        self.job = Some(Job::Post {
            rx,
            index,
            composer: confirm.composer,
            added,
        });
    }

    fn show_comments(&mut self) {
        if self.comments.is_empty() {
            self.toast("No comments yet: select lines with v, then press C");
            return;
        }
        self.dialog = Dialog::Comments { selected: 0 };
    }

    fn jump_to(&mut self, anchor: &Anchor) {
        let diff = &self.loaded.diff;
        let Some(first) = diff.row_for(&anchor.file, anchor.side, anchor.start_line) else {
            self.fail(format!(
                "{} is not in this diff. Reload with r, then try again.",
                anchor.label()
            ));
            return;
        };
        let last = diff
            .row_for(&anchor.file, anchor.side, anchor.end_line)
            .unwrap_or(first)
            .max(first);
        if let Some(file) = diff.file_index(first) {
            self.set_folded(file, false);
        }
        let lines = self.split.lines_for_rows(first, last);
        let Some((first, last)) = lines else {
            self.fail(format!(
                "{} is not in this diff. Reload with r, then try again.",
                anchor.label()
            ));
            return;
        };
        self.visual = false;
        self.cursor = first;
        self.selection = Some(first);
        self.focus = Some((first, last));
        self.follow_focus(first, last);
        self.toast(anchor.label());
    }

    fn copy_payload(&mut self, all: bool) {
        if all {
            if self.comments.is_empty() {
                self.toast("No comments to copy yet");
                return;
            }
            let mut text = String::new();
            for saved in &self.comments {
                let _ = writeln!(
                    text,
                    "> {} {}\n\n```diff\n{}\n```\n\n{}\n",
                    saved.comment.anchor.file,
                    saved.comment.anchor.range_label(),
                    self.loaded.diff.snippet(
                        self.loaded
                            .diff
                            .row_for(
                                &saved.comment.anchor.file,
                                saved.comment.anchor.side,
                                saved.comment.anchor.start_line
                            )
                            .unwrap_or(0),
                        self.loaded
                            .diff
                            .row_for(
                                &saved.comment.anchor.file,
                                saved.comment.anchor.side,
                                saved.comment.anchor.end_line
                            )
                            .unwrap_or(0),
                    ),
                    saved.comment.body
                );
            }
            self.copy = Some(text);
            self.toast(format!("Copied {} comments", self.comments.len()));
            return;
        }
        let (start, end) = self.range();
        let rows = self.split.comment_rows(start, end);
        let range = rows.and_then(|(first, last)| self.loaded.diff.comment_range(first, last));
        if let Some((first, last, anchor)) = range {
            self.copy = Some(payload(&self.loaded.diff, &anchor, first, last, ""));
            self.toast(format!("Copied {} {}", anchor.file, anchor.range_label()));
        } else {
            self.fail("That range has no code line to copy. Move to a changed line, then press y.");
        }
    }

    fn remember(&self) -> Option<(String, Side, u32)> {
        let diff = &self.loaded.diff;
        let row = diff.rows().get(self.current_row()?)?;
        if !row.kind.is_content() {
            return None;
        }
        let side = if row.kind == RowKind::Removed {
            Side::Old
        } else {
            Side::New
        };
        let line = match side {
            Side::Old => row.old,
            Side::New => row.new,
        };
        (line > 0).then(|| (diff.path(row.file as usize, side).to_owned(), side, line))
    }

    fn start_load(&mut self, request: Request, message: Option<&str>) {
        let remember = self.remember();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(request.load());
        });
        self.job = Some(Job::Load {
            rx,
            remember,
            background: message.is_none(),
        });
        self.polled = Instant::now();
        if let Some(message) = message {
            self.dialog = Dialog::Loading(message.to_owned());
        }
    }

    fn reload(&mut self) {
        self.start_load(self.loaded.request.clone(), Some("Reloading the diff…"));
    }

    fn other_side(&mut self) {
        match self.loaded.request.other_side() {
            Some(request) => {
                let message = request.label();
                self.start_load(request, Some(&format!("Loading {message}…")));
            }
            None => self.toast("This view has no second side"),
        }
    }

    fn remove(&mut self, index: usize) {
        if index >= self.comments.len() {
            return;
        }
        let saved = self.comments.remove(index);
        if saved.id >= 0 {
            let result = self
                .store
                .as_ref()
                .map(|store| store.delete_diff_comment(saved.id));
            if let Some(Err(e)) = result {
                self.store_failure(e);
            }
        }
        self.rebuild_marks();
    }
}

// PART3

impl App {
    fn draw(&mut self, f: &mut Frame) {
        self.new_highlights = HIGHLIGHT_BUDGET;
        let area = f.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.form() == Some(OutlineForm::Sidebar) && area.width < OUTLINE_MIN_SCREEN {
            self.outline = None;
        }
        let beside = match self.form() {
            Some(OutlineForm::Sidebar) => outline::sidebar(
                self.outline_columns,
                area.width,
                OUTLINE_MIN_SCREEN - OUTLINE_COLUMNS,
            ),
            _ => 0,
        };
        let body = Rect::new(
            area.x + beside,
            area.y,
            area.width - beside,
            area.height.saturating_sub(1),
        );
        self.view = body;
        if self.left > 0 {
            self.left = self.left.min(self.max_left());
        }
        self.follow_cursor();
        // The cursor also moves without a key: by a reload.
        self.sync_outline();
        if let Some(selected) = self.overview {
            self.draw_overview(f, body, selected);
        } else {
            self.draw_rows(f, body);
        }
        if beside > 0 {
            self.draw_outline_side(f, Rect::new(area.x, area.y, beside, body.height));
        }
        self.draw_note_editor(f, body);
        self.draw_footer(
            f,
            Rect::new(
                area.x,
                area.y + body.height,
                area.width,
                area.height.saturating_sub(body.height),
            ),
        );
        self.draw_composer(f, body);
        if let Some(kind) = self.dialog_kind() {
            self.draw_dialog(f, area, kind);
        }
        let ink = self.look.ink();
        if let Some(news) = &mut self.news {
            news.draw(f, ink);
        }
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let line = if let Some(toast) = &self.toast {
            let style = if toast.error {
                Style::default().fg(REMOVED_FG)
            } else {
                Style::default().fg(DIM)
            };
            let text = clip(&toast.text, area.width.saturating_sub(2) as usize);
            Line::from(Span::styled(format!(" {text}"), style))
        } else if self.note_editor.is_some() {
            Line::from(Span::styled(
                clip(
                    " enter save · alt+enter new line · empty clears it · esc discard",
                    area.width as usize,
                ),
                Style::default().fg(DIM),
            ))
        } else if self.overview.is_some() {
            let unsent = self.unsent();
            let right = match unsent {
                0 => String::new(),
                1 => "1 note ".to_owned(),
                n => format!("{n} notes "),
            };
            let room = (area.width as usize).saturating_sub(cells(&right) + 1);
            let mut spans = hints(
                &[
                    ("enter", "open"),
                    ("c", "note"),
                    ("w", "viewed"),
                    ("S", "send"),
                    ("C", "whole change"),
                    ("?", "keys"),
                ],
                room,
            );
            let used: usize = spans.iter().map(|span| cells(&span.content)).sum();
            spans.push(Span::raw(" ".repeat(
                (area.width as usize).saturating_sub(used + cells(&right)),
            )));
            spans.push(Span::styled(right, Style::default().fg(ACCENT)));
            Line::from(spans)
        } else if let Some(composer) = &self.composer {
            let keys = match composer.purpose {
                Purpose::Comment => {
                    " enter comment · ctrl+enter send · alt+enter line · ctrl+g github · esc cancel"
                }
                Purpose::Ask => {
                    " ask the agent about these lines · enter send · alt+enter line · esc cancel"
                }
            };
            Line::from(Span::styled(
                clip(keys, area.width as usize),
                Style::default().fg(DIM),
            ))
        } else {
            self.status_line(area.width)
        };
        if let Some(ink) = self.look.ink() {
            art::surface(f.buffer_mut(), area, ink);
        }
        f.render_widget(Paragraph::new(line), area);
    }

    fn status_line(&self, width: u16) -> Line<'static> {
        let diff = &self.loaded.diff;
        let Some(row) = self.current_row().and_then(|row| diff.rows().get(row)) else {
            return Line::from(Span::raw(" "));
        };
        let mut left = String::new();
        if let Some(file) = diff.file(row.file as usize) {
            left.push_str(file.path());
        }
        if row.kind.is_content() {
            let line = if row.kind == RowKind::Removed {
                row.old
            } else {
                row.new
            };
            left.push(':');
            left.push_str(&line.to_string());
        }
        if self.selection.is_some() {
            let (first, last) = self.range();
            let lines = self
                .split
                .lines()
                .get(first..=last.min(self.split.lines().len().saturating_sub(1)))
                .map(|lines| lines.iter().filter(|line| line.is_content()).count())
                .unwrap_or(0);
            let mode = if self.visual {
                "visual · "
            } else {
                "selected · "
            };
            let unit = if lines == 1 { "line" } else { "lines" };
            left.push_str(&format!("  {mode}{lines} {unit} · c comment"));
        }
        let (added, removed) = totals(diff);
        let unsent = self.unsent();
        let review = if unsent > 0 {
            format!("{unsent} unsent · S send   ")
        } else if self.sends == Sends::Return {
            "tab files · S send   ".to_owned()
        } else {
            String::new()
        };
        let stats = if self.tabs {
            format!("{review}1 summary · 2 timeline · 3 comments · 5 checks   +{added} −{removed} ")
        } else {
            format!("{review}+{added} −{removed} ")
        };
        let left_width = (width as usize).saturating_sub(cells(&stats) + 1);
        let left = clip(&left, left_width);
        let pad = (width as usize).saturating_sub(1 + cells(&left) + cells(&stats));
        Line::from(vec![
            Span::styled(format!(" {left}"), Style::default().fg(DIM)),
            Span::raw(" ".repeat(pad)),
            Span::styled(stats, Style::default().fg(DIM)),
        ])
    }

    fn draw_rows(&mut self, f: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let diff = &self.loaded.diff;
        if diff.rows().is_empty() || self.split.lines().is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " This diff is empty: nothing changed on this side.",
                    Style::default().fg(DIM),
                ))),
                area,
            );
            return;
        }
        // The file header stays pinned at the top while its lines scroll under
        // it, so a file's name is always visible no matter how the view moved.
        let pinned = self.pinned_header();
        let offset = usize::from(pinned.is_some());
        let height = (area.height as usize).saturating_sub(offset);
        let end = (self.scroll + height).min(self.split.lines().len());
        let mut lines = Vec::with_capacity(end.saturating_sub(self.scroll) + offset);
        // File headers, by screen row and the width of their text.
        let mut headers = Vec::new();
        if let Some(index) = pinned
            && let Some(row) = self.split.lines()[index].band
        {
            let mut drawn = self.row_line(row);
            if self.split.lines()[index].folded {
                fold_heading(&mut drawn);
            }
            headers.push((0, drawn.width() as u16));
            lines.push(drawn);
        }
        for index in self.scroll..end {
            let line = self.split.lines()[index];
            let mut drawn = match line.band {
                Some(row) => self.row_line(row),
                None => self.split_content(index, area.width),
            };
            if let Some(row) = line.band
                && self.loaded.diff.rows()[row].kind == RowKind::File
            {
                if line.folded {
                    fold_heading(&mut drawn);
                }
                if index == self.cursor {
                    // The cursor on a folded heading: a full-width bar, drawn
                    // over the ink so it shows.
                    let width = drawn.width();
                    drawn.spans.push(Span::raw(
                        " ".repeat((area.width as usize).saturating_sub(width)),
                    ));
                    drawn = drawn.patch_style(Style::default().bg(self.palette.cursor));
                } else {
                    headers.push((lines.len() as u16, drawn.width() as u16));
                }
            }
            lines.push(drawn);
        }
        let drawn = lines.len() as u16;
        f.render_widget(Paragraph::new(lines), area);
        // Each file opens with a tinted bar whose text trails off in dither.
        let Some(ink) = self.look.ink() else {
            return;
        };
        if drawn < area.height {
            let rest = Rect::new(
                area.x,
                area.y + drawn + 1,
                area.width,
                area.height - drawn - 1,
            );
            art::end(f.buffer_mut(), rest, ink);
        }
        for (y, used) in headers {
            let row = Rect::new(area.x, area.y + y as u16, area.width, 1);
            art::surface(f.buffer_mut(), row, ink);
            let gap = used.saturating_add(2).min(row.width);
            let trail = Rect::new(row.x + gap, row.y, row.width - gap, 1);
            art::trail(f.buffer_mut(), trail, ink);
        }
    }

    fn row_line(&mut self, index: usize) -> Line<'static> {
        let diff = &self.loaded.diff;
        let row = diff.rows()[index];
        match row.kind {
            RowKind::File => {
                let file = diff.file(row.file as usize);
                let (added, removed) = file.map_or((0, 0), |file| (file.added, file.removed));
                let path = file.map(|file| file.display()).unwrap_or_default();
                let note = file.and_then(|file| file.note.clone());
                // A viewed file's heading is muted, folded or not.
                let viewed = self.viewed.contains(&(row.file as usize));
                let (path_style, added_style, removed_style) = if viewed {
                    let muted = Style::default().fg(DIM);
                    (muted, muted, muted)
                } else {
                    (
                        Style::default().add_modifier(Modifier::BOLD),
                        Style::default().fg(ADDED_FG),
                        Style::default().fg(REMOVED_FG),
                    )
                };
                let mut spans = vec![
                    Span::styled(
                        " ▎ ",
                        Style::default().fg(if viewed { DIM } else { ACCENT }),
                    ),
                    Span::styled(path, path_style),
                ];
                if added > 0 {
                    spans.push(Span::styled(format!(" +{added}"), added_style));
                }
                if removed > 0 {
                    spans.push(Span::styled(format!(" −{removed}"), removed_style));
                }
                if viewed {
                    spans.push(Span::styled("  ✓ viewed", Style::default().fg(DIM)));
                }
                if let Some(note) = note {
                    spans.push(Span::styled(format!("  {note}"), Style::default().fg(DIM)));
                }
                Line::from(spans)
            }
            RowKind::Hunk => {
                let gap = diff
                    .file(row.file as usize)
                    .and_then(|file| file.hunks.get(row.hunk as usize))
                    .map_or(0, |hunk| hunk.gap_before);
                let note = hunk_note(diff.row_text(index));
                let label = if gap == 0 {
                    "⋯".to_owned()
                } else {
                    format!("⋯ {gap} unmodified line{}", if gap == 1 { "" } else { "s" })
                };
                let mut spans = vec![
                    Span::styled(" ▏ ", Style::default().fg(DIM)),
                    Span::styled(label, Style::default().fg(DIM)),
                ];
                if let Some(note) = note {
                    spans.push(Span::styled(
                        format!("  ·  {note}"),
                        Style::default().fg(DIM),
                    ));
                }
                Line::from(spans)
            }
            RowKind::Meta | RowKind::NoNewline => Line::from(Span::styled(
                diff.row_text(index).to_owned(),
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            )),
            RowKind::Context | RowKind::Added | RowKind::Removed => Line::default(),
        }
    }

    fn split_content(&mut self, line_index: usize, width: u16) -> Line<'static> {
        let Some(line) = self.split.lines().get(line_index).copied() else {
            return Line::default();
        };
        if width < 3 {
            return Line::default();
        }
        let current = line_index == self.cursor;
        let selected = self.selection.is_some_and(|anchor| {
            line_index >= anchor.min(self.cursor) && line_index <= anchor.max(self.cursor)
        });
        let focused = self
            .focus
            .is_some_and(|(first, last)| line_index >= first && line_index <= last);
        let mut spans = Vec::new();
        let side = SideState {
            current,
            highlighted: focused || selected,
        };
        if self.layout == Layout::Inline {
            // A context row is on both sides: draw it once, with both numbers.
            let (row, drawn) = match (line.left, line.right) {
                (_, Some(row)) => (row, Side::New),
                (Some(row), None) => (row, Side::Old),
                (None, None) => return Line::default(),
            };
            self.side_line(&mut spans, Some(row), drawn, width, side, true);
            return Line::from(spans);
        }
        let (left_width, right_width) = self.layout.pane_widths(width);
        self.side_line(&mut spans, line.left, Side::Old, left_width, side, false);
        let rule_style = if side.highlighted {
            Style::default().fg(ACCENT).bg(self.palette.focus)
        } else if current {
            Style::default().fg(ACCENT).bg(self.palette.cursor)
        } else {
            Style::default().fg(DIM)
        };
        spans.push(Span::styled("│", rule_style));
        self.side_line(&mut spans, line.right, Side::New, right_width, side, false);
        Line::from(spans)
    }

    /// Draws one half of a split line, or a whole inline one (`both`): a
    /// change or comment marker, the line number (inline: old and new), then
    /// the code.
    fn side_line(
        &mut self,
        spans: &mut Vec<Span<'static>>,
        row: Option<usize>,
        side: Side,
        width: u16,
        state: SideState,
        both: bool,
    ) {
        if width == 0 {
            return;
        }
        let number = self.number_width();
        let Some(index) = row else {
            let style = match self.empty_bg(state) {
                Some(bg) => Style::default().bg(bg),
                None => Style::default(),
            };
            spans.push(Span::styled(" ".repeat(width as usize), style));
            return;
        };
        let diff_row = self.loaded.diff.rows()[index];
        let tint = match (diff_row.kind, side) {
            (RowKind::Added, Side::New) => Some(self.palette.added),
            (RowKind::Removed, Side::Old) => Some(self.palette.removed),
            _ => None,
        };
        let bg = if state.highlighted {
            Some(self.palette.focus)
        } else if state.current {
            Some(match (diff_row.kind, side) {
                (RowKind::Added, Side::New) => self.palette.added_cursor,
                (RowKind::Removed, Side::Old) => self.palette.removed_cursor,
                _ => self.palette.cursor,
            })
        } else {
            tint
        };
        let base = Style::default().bg(bg.unwrap_or(Color::Reset));
        let mark = self.marks.get(&index).copied();
        let (lead, lead_style) = match (mark, diff_row.kind) {
            (Some(mark), _) => {
                let color = match mark {
                    State::Draft => DIM,
                    State::Agent => ACCENT,
                    State::Github => ADDED_FG,
                };
                ("●", base.fg(color))
            }
            (None, _) if state.current => ("▌", base.fg(ACCENT)),
            (None, RowKind::Added) if side == Side::New => ("▌", base.fg(ADDED_FG)),
            (None, RowKind::Removed) if side == Side::Old => ("▌", base.fg(REMOVED_FG)),
            _ => (" ", base),
        };
        spans.push(Span::styled(lead, lead_style));

        let numbered = |label: u32| {
            if label > 0 {
                format!("{label:>number$} ")
            } else {
                " ".repeat(number + 1)
            }
        };
        let label = match (both, side) {
            (true, _) => numbered(diff_row.old) + &numbered(diff_row.new),
            (false, Side::Old) => numbered(diff_row.old),
            (false, Side::New) => numbered(diff_row.new),
        };
        let gutter_style = if state.current {
            Style::default()
                .fg(ACCENT)
                .bg(bg.unwrap_or(Color::Reset))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM).bg(bg.unwrap_or(Color::Reset))
        };
        spans.push(Span::styled(label, gutter_style));

        let text = self.loaded.diff.content(index).to_owned();
        let kinds = self.kinds_for(index);
        let line_index = self.hunk_line(index);
        let kinds_line = kinds
            .as_ref()
            .and_then(|lines| lines.get(line_index))
            .map(Vec::as_slice);
        let emphasis = self.emphasis(index);
        let word = match (diff_row.kind, side) {
            (RowKind::Added, Side::New) => self.palette.added_word,
            (RowKind::Removed, Side::Old) => self.palette.removed_word,
            _ => Color::Reset,
        };
        let gutter = self.layout.gutter(number);
        let available = (width as usize).saturating_sub(gutter);
        let overlay = state.highlighted.then_some(self.palette.focus);
        let used = text_spans(
            spans, &text, self.left, available, base, kinds_line, &emphasis, word, overlay,
        );
        let filled = (gutter + used).min(width as usize);
        if filled < width as usize {
            spans.push(Span::styled(
                " ".repeat(width as usize - filled),
                Style::default().bg(bg.unwrap_or(Color::Reset)),
            ));
        }
    }

    fn empty_bg(&self, state: SideState) -> Option<Color> {
        if state.highlighted {
            Some(self.palette.focus)
        } else {
            state.current.then_some(self.palette.cursor)
        }
    }

    fn number_width(&self) -> usize {
        self.loaded.diff.max_line().to_string().len().max(3)
    }

    /// The index of `row` among its hunk's content rows, matching the lines
    /// the lexer returned.
    fn hunk_line(&self, row: usize) -> usize {
        let diff = &self.loaded.diff;
        let Some(hunk) = diff
            .rows()
            .get(row)
            .and_then(|row| diff.file(row.file as usize)?.hunks.get(row.hunk as usize))
        else {
            return 0;
        };
        (hunk.first..row)
            .filter(|&index| diff.rows()[index].kind.is_content())
            .count()
    }

    /// Lexes the hunk a row belongs to, at most once per frame, and returns
    /// one kind per character for each of its content lines.
    fn kinds_for(&mut self, row_index: usize) -> Option<Rc<Vec<Vec<syntax::Kind>>>> {
        let row = *self.loaded.diff.rows().get(row_index)?;
        if !row.kind.is_content() || row.hunk == u32::MAX {
            return None;
        }
        let key = (row.file, row.hunk);
        if let Some(kinds) = self.highlight.get(&key) {
            return Some(kinds.clone());
        }
        if self.new_highlights == 0 {
            return None;
        }
        let file = self.loaded.diff.file(row.file as usize)?;
        let hunk = file.hunks.get(row.hunk as usize)?;
        let mut code = String::new();
        for index in hunk.first..hunk.end {
            if index > hunk.first {
                code.push('\n');
            }
            code.push_str(self.loaded.diff.content(index));
            if code.len() > HIGHLIGHT_BYTES {
                self.cache(key, Rc::new(Vec::new()));
                return None;
            }
        }
        let hint = Path::new(file.path())
            .extension()
            .and_then(|extension| extension.to_str());
        let kinds = syntax::kinds(&code, hint);
        if kinds.is_empty() {
            // No lexer for this file: remember that, so it stays plain
            // without being retried on every frame.
            self.cache(key, Rc::new(Vec::new()));
            return None;
        }
        let mut lines: Vec<Vec<syntax::Kind>> = Vec::new();
        let mut current = Vec::new();
        for (at, ch) in code.chars().enumerate() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
            } else {
                current.push(kinds.get(at).copied().unwrap_or(syntax::Kind::Plain));
            }
        }
        lines.push(current);
        let kinds = Rc::new(lines);
        self.cache(key, kinds.clone());
        Some(kinds)
    }

    fn cache(&mut self, key: (u32, u32), kinds: Rc<Vec<Vec<syntax::Kind>>>) {
        self.new_highlights = self.new_highlights.saturating_sub(1);
        if self.highlight.len() >= HIGHLIGHT_CACHE {
            self.highlight.clear();
        }
        self.highlight.insert(key, kinds);
    }

    /// The changed text of a removed or added line, as character ranges,
    /// against the line it is paired with. Empty when it has no partner.
    fn emphasis(&self, row_index: usize) -> Vec<std::ops::Range<usize>> {
        let diff = &self.loaded.diff;
        let Some(partner) = self.split.partner(diff, row_index) else {
            return Vec::new();
        };
        let (text, other) = (diff.content(row_index), diff.content(partner));
        match diff.rows()[row_index].kind {
            RowKind::Removed => linediff::Changes::between(text, other).old,
            _ => linediff::Changes::between(other, text).new,
        }
    }

    fn draw_composer(&mut self, f: &mut Frame, area: Rect) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        if area.height < 2 || area.width < 4 {
            return;
        }
        let width = area.width.saturating_sub(2);
        let text_width = width.saturating_sub(2);
        let (lines, cursor_line) = composer.wrap(text_width as usize);
        let visible = lines.len().clamp(1, 5).min(area.height as usize - 1);
        let height = visible as u16 + 1;
        let rect = Rect::new(area.x + 1, area.y + area.height - height, width, height);
        f.render_widget(Clear, rect);
        if let Some(ink) = self.look.ink() {
            art::surface(f.buffer_mut(), rect, ink);
        }
        let title = format!(
            " {} {} ",
            composer.anchor.file,
            composer.anchor.range_label()
        );
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(DIM))
            .title(Line::from(Span::styled(title, Style::default().fg(DIM))));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        if inner.height == 0 || text_width == 0 {
            return;
        }
        let rows = inner.height as usize;
        let mut view = self.composer_view as usize;
        if cursor_line < view {
            view = cursor_line;
        } else if cursor_line >= view + rows {
            view = cursor_line + 1 - rows;
        }
        view = view.min(lines.len().saturating_sub(rows));
        self.composer_view = view as u16;

        let text_area = Rect {
            x: inner.x + 1,
            y: inner.y,
            width: text_width,
            height: inner.height,
        };
        let mut rendered = Vec::new();
        for &(start, end) in lines.iter().skip(view).take(rows) {
            let mut spans = Vec::new();
            for (at, ch) in composer.text[start..end].iter().enumerate() {
                let cursor = start + at == composer.cursor;
                let style = if cursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(ch.to_string(), style));
            }
            if composer.cursor == end {
                spans.push(Span::styled(
                    " ",
                    Style::default().add_modifier(Modifier::REVERSED),
                ));
            }
            rendered.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(rendered), text_area);
    }

    fn draw_dialog(&mut self, f: &mut Frame, area: Rect, kind: DialogKind) {
        match kind {
            DialogKind::Help => draw_help(f, area, self.look.ink(), &self.keys),
            DialogKind::Loading => {
                let text = match &self.dialog {
                    Dialog::Loading(text) => text.clone(),
                    _ => String::new(),
                };
                panel(
                    f,
                    area,
                    self.look.ink(),
                    "loading",
                    vec![Line::from(Span::styled(text, Style::default().fg(DIM)))],
                    40,
                );
            }
            DialogKind::Error => {
                let text = match &self.dialog {
                    Dialog::Error(text) => text.clone(),
                    _ => String::new(),
                };
                let width = 74.min(area.width.saturating_sub(8)).max(20);
                let mut lines: Vec<Line> = wrap_text(&text, width.saturating_sub(6) as usize)
                    .into_iter()
                    .map(|line| Line::from(Span::styled(line, Style::default())))
                    .collect();
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Esc closes · r reloads",
                    Style::default().fg(DIM),
                )));
                panel(f, area, self.look.ink(), "could not finish", lines, width);
            }
            DialogKind::Outline => self.draw_outline_window(f, area),
            DialogKind::Comments => self.draw_comments(f, area),
            DialogKind::Files => self.draw_files(f, area),
            DialogKind::Confirm => self.draw_confirm(f, area),
            DialogKind::Verdict => self.draw_verdict(f, area),
        }
    }

    /// A minimal list of the changed files. Typing filters it, the cursor picks
    /// one, and enter opens it.
    fn draw_files(&self, f: &mut Frame, area: Rect) {
        let Dialog::Files { selected, query } = &self.dialog else {
            return;
        };
        let selected = *selected;
        let files = self.loaded.diff.files();
        let showing = filtered_files(&self.loaded.diff, query);
        let width = 66.min(area.width.saturating_sub(8)).max(24);
        let text_width = width.saturating_sub(6) as usize;
        let rows = (area.height as usize).saturating_sub(6).clamp(1, 14);
        let start = selected.saturating_sub(rows.saturating_sub(1));
        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(" / ", Style::default().fg(ACCENT)),
            if query.is_empty() {
                Span::styled("filter changed files", Style::default().fg(DIM))
            } else {
                Span::styled(query.clone(), Style::default())
            },
            Span::styled("▏", Style::default().fg(ACCENT)),
        ]));
        if showing.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no file matches",
                Style::default().fg(DIM),
            )));
        }
        for (offset, &index) in showing.iter().skip(start).take(rows).enumerate() {
            let file = &files[index];
            let chosen = start + offset == selected;
            let (status, status_color) = file_status(file);
            let stats = format!("+{} −{}", file.added, file.removed);
            let path_width = text_width.saturating_sub(cells(&stats) + 4);
            let path = clip(&file.display(), path_width);
            let pad = path_width.saturating_sub(cells(&path));
            let style = if chosen {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(if chosen { "▸ " } else { "  " }, style),
                Span::styled(status.to_string(), Style::default().fg(status_color)),
                Span::styled(format!(" {path}{}", " ".repeat(pad)), style),
                Span::styled(
                    format!("  {stats}"),
                    Style::default().fg(if chosen { ACCENT } else { DIM }),
                ),
            ]));
        }
        lines.push(Line::from(""));
        let shown = if showing.is_empty() {
            format!("no match · {}", files.len())
        } else if query.is_empty() {
            format!("{} of {}", selected + 1, files.len())
        } else {
            format!(
                "{} of {} · {} total",
                selected + 1,
                showing.len(),
                files.len()
            )
        };
        lines.push(Line::from(vec![
            Span::styled("↑↓ move · enter open · esc close", Style::default().fg(DIM)),
            Span::styled(format!("  {shown}"), Style::default().fg(DIM)),
        ]));
        panel(f, area, self.look.ink(), "files", lines, width);
    }

    fn draw_comments(&self, f: &mut Frame, area: Rect) {
        let Dialog::Comments { selected } = &self.dialog else {
            return;
        };
        let selected = *selected;
        let width = 80.min(area.width.saturating_sub(8)).max(24);
        let mut lines = Vec::new();
        for (index, saved) in self.comments.iter().enumerate() {
            let mark = if index == selected { "▸" } else { " " };
            let style = if index == selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let label = saved.comment.label();
            let state = saved.comment.state.label();
            let fixed = 6 + cells(&label) + cells(state);
            let body = clip(
                &saved.comment.body.replace('\n', " "),
                (width as usize).saturating_sub(fixed),
            );
            lines.push(Line::from(vec![
                Span::styled(mark, Style::default().fg(ACCENT)),
                Span::styled(format!(" {label}"), style),
                Span::styled(format!("  {body}"), Style::default().fg(DIM)),
                Span::styled(format!("  {state}"), Style::default().fg(DIM)),
            ]));
        }
        let max_rows = area.height.saturating_sub(5) as usize;
        if lines.len() > max_rows {
            lines.truncate(max_rows);
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "enter jump · a agent · g github · d delete · y copy · esc close",
            Style::default().fg(DIM),
        )));
        panel(f, area, self.look.ink(), "comments", lines, width);
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect) {
        let Dialog::Confirm(confirm) = &self.dialog else {
            return;
        };
        let width = 60.min(area.width.saturating_sub(8)).max(24);
        let target = self
            .loaded
            .github
            .as_ref()
            .map(|resource| resource.label())
            .unwrap_or_else(|| "the pull request for this branch".to_owned());
        let mut lines: Vec<Line> = wrap_text(
            &format!("Post this comment to {target}?"),
            width.saturating_sub(4) as usize,
        )
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::default())))
        .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            confirm.anchor.label(),
            Style::default().fg(DIM),
        )));
        for line in wrap_text(&confirm.body, width.saturating_sub(6) as usize)
            .into_iter()
            .take(6)
        {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(DIM),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("y", Style::default().fg(ADDED_FG)),
            Span::styled(" post    ", Style::default().fg(DIM)),
            Span::styled("n", Style::default().fg(REMOVED_FG)),
            Span::styled(" back to editing", Style::default().fg(DIM)),
        ]));
        panel(f, area, self.look.ink(), "post to github", lines, width);
    }

    fn has_pending_highlight(&self) -> bool {
        let diff = &self.loaded.diff;
        let end = (self.scroll + self.view.height as usize).min(self.split.lines().len());
        (self.scroll..end).any(|index| {
            self.split.lines()[index].rows().any(|row| {
                let row = diff.rows()[row];
                row.kind.is_content()
                    && row.hunk != u32::MAX
                    && !self.highlight.contains_key(&(row.file, row.hunk))
            })
        })
    }
}

/// Key hints, `key action` apart, as many as fit `room` columns.
fn hints(keys: &[(&str, &str)], room: usize) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (key, action) in keys {
        let width = cells(key) + 1 + cells(action) + 3;
        if used + width > room {
            break;
        }
        used += width;
        spans.push(Span::styled((*key).to_owned(), Style::default().fg(ACCENT)));
        spans.push(Span::styled(
            format!(" {action}   "),
            Style::default().fg(DIM),
        ));
    }
    spans
}

fn totals(diff: &Diff) -> (u32, u32) {
    diff.files().iter().fold((0, 0), |(added, removed), file| {
        (added + file.added, removed + file.removed)
    })
}

/// The one-letter change marker the file picker shows.
fn file_status(file: &File) -> (char, Color) {
    match (&file.old_path, &file.new_path) {
        (None, Some(_)) => ('A', ADDED_FG),
        (Some(_), None) => ('D', REMOVED_FG),
        (Some(old), Some(new)) if old != new => ('R', ACCENT),
        _ => ('M', DIM),
    }
}

/// The files the picker lists for a search. An empty search keeps every file.
fn filtered_files(diff: &Diff, query: &str) -> Vec<usize> {
    let needle = query.to_lowercase();
    diff.files()
        .iter()
        .enumerate()
        .filter(|(_, file)| needle.is_empty() || file.display().to_lowercase().contains(&needle))
        .map(|(index, _)| index)
        .collect()
}

/// The function or context Git prints after a hunk header.
fn hunk_note(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("@@")?;
    let at = rest.find("@@")?;
    let note = rest[at + 2..].trim();
    (!note.is_empty()).then_some(note)
}

fn cell_width(symbol: &str, column: usize) -> usize {
    if symbol == "\t" {
        4 - column % 4
    } else if symbol.contains(char::is_control) {
        0
    } else {
        symbol.cell_width() as usize
    }
}

fn cells(text: &str) -> usize {
    text.graphemes(true)
        .fold(0, |col, symbol| col + cell_width(symbol, col))
}

/// Appends `text` to `spans` one style run at a time, skipping `start` columns
/// and stopping after `avail` columns.
#[allow(clippy::too_many_arguments)]
fn text_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    start: usize,
    avail: usize,
    base: Style,
    kinds: Option<&[syntax::Kind]>,
    emphasis: &[std::ops::Range<usize>],
    word: Color,
    selection: Option<Color>,
) -> usize {
    let end = start.saturating_add(avail);
    let mut column = 0;
    let mut used = 0;
    let mut char_index = 0;
    for symbol in text.graphemes(true) {
        if column >= end {
            break;
        }
        let at = char_index;
        char_index += symbol.chars().count();
        let width = cell_width(symbol, column);
        let from = column;
        column += width;
        let visible = column.min(end).saturating_sub(from.max(start));
        if visible == 0 {
            continue;
        }
        let mut style = base;
        if let Some(kinds) = kinds
            && let Some(kind) = kinds.get(at)
        {
            style = style.patch(crate::render::code_style(*kind));
        }
        if emphasis.iter().any(|range| range.contains(&at)) {
            style = style.bg(word);
        }
        if let Some(selection) = selection {
            style = style.bg(selection);
        }
        if symbol == "\t" || visible < width {
            // A clipped wide glyph can't be drawn in half. Keep its visible cells in place.
            push_text(spans, style, &" ".repeat(visible));
        } else {
            push_text(spans, style, symbol);
        }
        used += visible;
    }
    used
}

fn push_text(spans: &mut Vec<Span<'static>>, style: Style, text: &str) {
    if let Some(last) = spans.last_mut()
        && last.style == style
        && let std::borrow::Cow::Owned(content) = &mut last.content
    {
        content.push_str(text);
        return;
    }
    spans.push(Span::styled(text.to_owned(), style));
}

/// Marks a file heading as folded: its bar turns into a closed arrow.
fn fold_heading(line: &mut Line<'static>) {
    if let Some(bar) = line.spans.first_mut() {
        bar.content = " ▸ ".into();
    }
}

/// Pads a band row to the full width with its background still drawn.
fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width.saturating_sub(1) && width > 0 {
            out.push('…');
            break;
        }
        out.push(ch);
        used += cells;
    }
    out
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && cells(&line) + 1 + cells(word) > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

/// The keys as `[keys.diff]` binds them, so a rebound key shows here too.
fn draw_help(f: &mut Frame, area: Rect, ink: Option<art::Ink>, keymap: &Keymap<DiffAction>) {
    use DiffAction as A;
    let keys = |actions: &[DiffAction]| keymap.labels(actions);
    let rows = [
        (keys(&[A::Down, A::Up]), "move between code lines"),
        (
            keys(&[A::HalfPageDown, A::HalfPageUp]),
            "half page down or up",
        ),
        (keys(&[A::PageDown, A::PageUp]), "page down or up"),
        (keys(&[A::First, A::Last]), "first or last line"),
        (
            keys(&[A::PreviousFile, A::NextFile]),
            "previous or next file",
        ),
        (
            keys(&[A::Fold, A::Unfold]),
            "fold this file to its heading, or open it",
        ),
        (keys(&[A::Files]), "files: filter by path, open one"),
        (
            keys(&[A::Outline, A::OutlineWindow]),
            "outline of every file beside the diff, or over it",
        ),
        (
            "< >".to_owned(),
            "outline open: narrow or widen its sidebar (or drag its edge)",
        ),
        (
            keys(&[A::Select]),
            "visual mode; move to select, esc clears",
        ),
        (keys(&[A::Comment]), "comment on the line or selection"),
        (
            keys(&[A::Ask]),
            "ask the agent about the line or selection (⇧⌘A in Rex)",
        ),
        (keys(&[A::Comments]), "comments: jump, resend, delete, post"),
        (
            keys(&[A::Overview]),
            "overview of every file, with notes and viewed marks",
        ),
        (keys(&[A::FileNote]), "note on this whole file"),
        (keys(&[A::GlobalNote]), "global note on the whole change"),
        (
            keys(&[A::Viewed]),
            "mark this file viewed and fold it (again: not viewed)",
        ),
        (
            keys(&[A::SendReview]),
            "send drafts and notes as one review, with a verdict",
        ),
        (
            keys(&[A::Copy, A::CopyAll]),
            "copy this range or every comment",
        ),
        (
            keys(&[A::OtherSide, A::Reload]),
            "other side of the diff, or reload",
        ),
        (keys(&[A::Layout]), "split or inline layout"),
        (
            keys(&[A::ScrollLeft, A::ScrollRight]),
            "scroll sideways with keys, wheel or trackpad",
        ),
        (keys(&[A::ScrollReset]), "scroll back to the left edge"),
        (keys(&[A::Help, A::Quit]), "this help, quit"),
    ];
    let column = rows
        .iter()
        .map(|(key, _)| key.chars().count() + 1)
        .max()
        .unwrap_or(0)
        .max(16);
    let mut lines: Vec<Line> = rows
        .into_iter()
        // An action left with no key has nothing to show.
        .filter(|(key, _)| !key.is_empty())
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<column$}"), Style::default().fg(ACCENT)),
                Span::styled(what, Style::default()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Comment, write, Enter saves the comment on that line.",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " Ctrl+Enter pastes it into the agent pane · Alt+Enter adds a line.",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " Ctrl+G posts to GitHub · Esc cancels. `leaf comments` lists them.",
        Style::default().fg(DIM),
    )));
    panel(f, area, ink, "keys", lines, 78);
}

fn panel(
    f: &mut Frame,
    area: Rect,
    ink: Option<art::Ink>,
    title: &str,
    lines: Vec<Line<'static>>,
    width: u16,
) -> Rect {
    let width = width.saturating_add(6).min(area.width);
    let height = (lines.len() as u16).saturating_add(2).min(area.height);
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
        .border_style(Style::default().fg(DIM))
        .title(Line::from(format!(" {title} ")).style(Style::default().fg(DIM)));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.height > 0 {
        let body = Rect {
            x: inner.x + 2,
            y: inner.y,
            width: inner.width.saturating_sub(4),
            height: inner.height,
        };
        f.render_widget(Paragraph::new(lines), body);
    }
    rect
}

// The outline: the diff's files as a tree beside the diff or over it, and the
// way between its entries.

impl App {
    fn form(&self) -> Option<OutlineForm> {
        debug_assert!(
            self.outline.is_some() || !matches!(self.dialog, Dialog::Outline),
            "the outline window is open with no tree"
        );
        self.outline
            .as_ref()
            .map(|open| open.form(matches!(self.dialog, Dialog::Outline)))
    }

    fn close_outline(&mut self) {
        self.outline = None;
        if matches!(self.dialog, Dialog::Outline) {
            self.dialog = Dialog::None;
        }
    }

    /// `t`/`T`: toggle the outline beside the diff or over it. The form
    /// already shown closes; the other form switches to it, tree and all.
    fn toggle_outline(&mut self, form: OutlineForm) {
        if self.form() == Some(form) {
            self.close_outline();
            return;
        }
        if form == OutlineForm::Sidebar && self.view.width < OUTLINE_MIN_SCREEN {
            self.toast("The screen is too narrow for the outline");
            return;
        }
        if self.outline.is_none() {
            let nodes = outline::from_diff_files(self.loaded.diff.files());
            if nodes.is_empty() {
                self.toast("This diff has no files");
                return;
            }
            self.outline = Some(OpenOutline::new(outline::Outline::new(nodes)));
        }
        // Only the outline's own window can be open here: any other dialog
        // would have taken the key.
        self.dialog = match form {
            OutlineForm::Sidebar => Dialog::None,
            OutlineForm::Window => Dialog::Outline,
        };
    }

    /// The cursor also moves without a key: by a reload. The tree itself is brought
    /// in where the reload lands.
    fn sync_outline(&mut self) {
        let here = self.outline_here();
        if let Some(open) = self.outline.as_mut() {
            open.follows(here);
        }
    }

    /// The cursor's file; with no file under it, the first entry that goes somewhere.
    fn outline_here(&self) -> Option<usize> {
        let open = self.outline.as_ref()?;
        self.current_file()
            .and_then(|file| open.tree.at(outline::Locator::Diff(file)))
            .or_else(|| {
                open.tree
                    .nodes
                    .iter()
                    .position(|node| node.locator.is_some())
            })
    }

    fn outline_go(&mut self, node: usize) {
        let locator = self
            .outline
            .as_ref()
            .and_then(|open| open.tree.nodes[node].locator);
        if let Some(outline::Locator::Diff(index)) = locator {
            self.open_file(index);
            // The file starts at the top of the diff, its name and first hunk above the cursor.
            self.scroll = self.heading_start(self.cursor).min(self.max_scroll());
        }
    }

    fn outline_key(&mut self, key: KeyEvent) -> bool {
        // Before the next draw: a jump earlier in this batch of keys must be seen by the next.
        self.sync_outline();
        let Some((taken, ask)) = self.outline.as_mut().map(|open| open.key(&key)) else {
            return false;
        };
        match ask {
            Some(Ask::Close) => self.close_outline(),
            Some(Ask::Show(form)) => self.toggle_outline(form),
            Some(Ask::Go(Some(node))) => self.outline_go(node),
            Some(Ask::Resize { wider }) => {
                self.outline_columns = outline::resized(self.outline_columns, wider);
            }
            Some(Ask::Go(None)) | None => {}
        }
        taken
    }

    fn draw_outline_side(&self, f: &mut Frame, area: Rect) {
        let here = self.outline_here();
        let Some(open) = self.outline.as_ref() else {
            return;
        };
        let rows = (area.height as usize).saturating_sub(2);
        let lines = self.outline_lines(open, here, rows);
        let body = Rect {
            x: area.x + 1,
            width: area.width.saturating_sub(2),
            ..area
        };
        f.render_widget(Paragraph::new(lines), body);
        let edge = Rect {
            x: (area.x + area.width).saturating_sub(1),
            width: 1,
            ..area
        };
        f.render_widget(Paragraph::new(outline::edge(area.height)), edge);
    }

    fn draw_outline_window(&self, f: &mut Frame, area: Rect) {
        let here = self.outline_here();
        let Some(open) = self.outline.as_ref() else {
            return;
        };
        let rows = (area.height as usize).saturating_sub(8);
        let lines = self.outline_lines(open, here, rows);
        let width = lines
            .iter()
            .map(|line| line.width() as u16)
            .max()
            .unwrap_or(0);
        panel(f, area, self.look.ink(), "Outline", lines, width);
    }

    fn outline_lines(
        &self,
        open: &OpenOutline,
        here: Option<usize>,
        rows: usize,
    ) -> Vec<Line<'static>> {
        open.lines(
            here,
            rows,
            "No files in this diff.",
            |entry, style, picked| match entry.locator {
                Some(outline::Locator::Diff(index)) => {
                    self.file_spans(index, &entry.label, style, picked)
                }
                _ => vec![Span::styled(format!("{}/", entry.label), style)],
            },
        )
    }

    /// The notes come from the file's own index, not from a row on the panel.
    fn file_spans(
        &self,
        index: usize,
        label: &str,
        style: Style,
        picked: bool,
    ) -> Vec<Span<'static>> {
        let diff = &self.loaded.diff;
        let Some(file) = diff.file(index) else {
            return vec![Span::styled(label.to_owned(), style)];
        };
        let viewed = self.notes.is_viewed(diff, index);
        let old = file.old_path.as_deref().filter(|old| *old != file.path());
        let comments =
            self.draft_comments_in(file.path()) + old.map_or(0, |old| self.draft_comments_in(old));
        let notes = comments + usize::from(self.notes.files.contains_key(file.path()));
        let mark = match (viewed, notes > 0) {
            (_, true) => "●",
            (true, false) => "✓",
            (false, false) => " ",
        };
        let (status, color) = file_status(file);
        let tint = |color: Color| {
            if picked { style } else { style.fg(color) }
        };
        let mut spans = vec![
            Span::styled(
                format!("{mark} "),
                tint(if notes > 0 { ACCENT } else { DIM }),
            ),
            Span::styled(format!("{status} "), tint(color)),
            Span::styled(label.to_owned(), if viewed { tint(DIM) } else { style }),
        ];
        if file.added > 0 || file.removed > 0 {
            spans.push(Span::styled(format!(" +{}", file.added), tint(ADDED_FG)));
            spans.push(Span::styled(
                format!(" −{}", file.removed),
                tint(REMOVED_FG),
            ));
        }
        if let Some(note) = &file.note {
            spans.push(Span::styled(format!(" · {note}"), tint(DIM)));
        }
        spans
    }
}

impl App {
    fn wait_duration(&self) -> Option<Duration> {
        let wait = if self.job.is_some() {
            Some(Duration::from_millis(40))
        } else if let Some(toast) = &self.toast {
            Some(toast.until.saturating_duration_since(Instant::now()))
        } else if self.has_pending_highlight() {
            Some(Duration::from_millis(16))
        } else {
            None
        };
        // Wakes in time to follow the theme and the UI setting.
        let due = self.look.due().min(self.loaded.request.refresh_interval());
        Some(wait.map_or(due, |wait| wait.min(due)))
    }
}

// Margin: the overview of every file, file and global notes, viewed marks,
// and the one review `S` sends.

impl App {
    /// How many notes the review would send now.
    fn unsent(&self) -> usize {
        self.notes.count()
            + self
                .comments
                .iter()
                .filter(|saved| saved.comment.state == State::Draft)
                .count()
    }

    fn persist_notes(&mut self) {
        let json = self.notes.to_json();
        (self.save_notes)(json.as_deref());
    }

    /// Keeps file notes under the paths the diff shows now.
    fn follow_renames(&mut self) {
        if self.notes.follow_renames(&self.loaded.diff) {
            self.persist_notes();
        }
    }

    /// `q` reviewing with notes unsent asks for a second `q`, so they aren't
    /// left behind by accident. They're kept either way.
    fn ask_quit(&mut self) {
        let unsent = self.unsent();
        if self.sends == Sends::Return && unsent > 0 && !self.quit_armed {
            self.quit_armed = true;
            let notes = if unsent == 1 { "1 note" } else { "notes" };
            self.toast(format!(
                "{notes} not sent yet: S sends the review, q again quits and keeps them"
            ));
        } else {
            self.quit = true;
        }
    }

    fn show_overview(&mut self) {
        if self.loaded.diff.files().is_empty() {
            self.toast("This diff has no files");
            return;
        }
        let row = self.current_file().map_or(1, |file| file + 1);
        self.overview = Some(row);
    }

    fn overview_rows(&self) -> usize {
        self.loaded.diff.files().len() + 1
    }

    /// The file the overview row picks, or `None` on the global row.
    fn overview_file(&self) -> Option<usize> {
        self.overview.and_then(|row| row.checked_sub(1))
    }

    fn overview_key(&mut self, key: KeyEvent) {
        let Some(selected) = self.overview else {
            return;
        };
        let last = self.overview_rows() - 1;
        if !matches!(key.code, KeyCode::Char('q')) {
            self.quit_armed = false;
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.overview = Some((selected + 1).min(last)),
            KeyCode::Char('k') | KeyCode::Up => self.overview = Some(selected.saturating_sub(1)),
            KeyCode::Char('g') | KeyCode::Home => self.overview = Some(0),
            KeyCode::Char('G') | KeyCode::End => self.overview = Some(last),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => match self.overview_file() {
                Some(file) => {
                    self.overview = None;
                    self.open_file(file);
                }
                None => self.edit_global_note(),
            },
            KeyCode::Char('c') => match self.overview_file() {
                Some(file) => self.edit_file_note(Some(file)),
                None => self.edit_global_note(),
            },
            KeyCode::Char('C') | KeyCode::Char('M') => self.edit_global_note(),
            KeyCode::Char('w' | 'V') => {
                if let Some(file) = self.overview_file() {
                    self.toggle_viewed(Some(file));
                }
            }
            KeyCode::Char('x') => match self.overview_file() {
                Some(file) => {
                    let path = self.loaded.diff.files()[file].path().to_owned();
                    if self.notes.files.remove(&path).is_some() {
                        self.persist_notes();
                        self.toast("File note deleted");
                    }
                }
                None => {
                    if self.notes.global.take().is_some() {
                        self.persist_notes();
                        self.toast("Global note deleted");
                    }
                }
            },
            KeyCode::Char('S') => self.open_verdict(),
            KeyCode::Char('m') => self.show_comments(),
            KeyCode::Char('?') => self.dialog = Dialog::Help,
            KeyCode::Tab | KeyCode::Char('O') | KeyCode::Esc => {
                if self.sends == Sends::Return && key.code == KeyCode::Esc {
                    self.ask_quit();
                } else {
                    self.overview = None;
                }
            }
            KeyCode::Char('q') => self.ask_quit(),
            _ => {}
        }
    }

    /// Writes the note on file `file`, or the file under the cursor.
    fn edit_file_note(&mut self, file: Option<usize>) {
        let Some(file) = file.or_else(|| self.current_file()) else {
            self.toast("Move to a file first");
            return;
        };
        let Some(path) = self.loaded.diff.file(file).map(|f| f.path().to_owned()) else {
            return;
        };
        let text = self.notes.files.get(&path).cloned().unwrap_or_default();
        self.note_editor = Some(NoteEditor {
            target: NoteTarget::File(path),
            text,
        });
    }

    fn edit_global_note(&mut self) {
        self.note_editor = Some(NoteEditor {
            target: NoteTarget::Global,
            text: self.notes.global.clone().unwrap_or_default(),
        });
    }

    fn editor_key(&mut self, key: KeyEvent) {
        let Some(editor) = self.note_editor.as_mut() else {
            return;
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.note_editor = None;
                self.toast("Note discarded");
            }
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                editor.text.push('\n')
            }
            KeyCode::Enter => {
                let Some(editor) = self.note_editor.take() else {
                    return;
                };
                let message = match &editor.target {
                    NoteTarget::Global => {
                        self.notes.set_global(&editor.text);
                        if self.notes.global.is_some() {
                            "Global note saved"
                        } else {
                            "Global note cleared"
                        }
                    }
                    NoteTarget::File(path) => {
                        self.notes.set_file(path, &editor.text);
                        if self.notes.files.contains_key(path) {
                            "File note saved"
                        } else {
                            "File note cleared"
                        }
                    }
                };
                self.persist_notes();
                self.toast(message);
            }
            KeyCode::Backspace if alt || ctrl => crate::comment::delete_word(&mut editor.text),
            KeyCode::Char('w') if ctrl => crate::comment::delete_word(&mut editor.text),
            KeyCode::Backspace => {
                editor.text.pop();
            }
            KeyCode::Char('u') if ctrl => editor.text.clear(),
            KeyCode::Char(c) if !ctrl => editor.text.push(c),
            _ => {}
        }
    }

    fn toggle_viewed(&mut self, file: Option<usize>) {
        let Some(file) = file.or_else(|| self.current_file()) else {
            return;
        };
        let viewed = self.notes.toggle_viewed(&self.loaded.diff, file);
        self.persist_notes();
        self.refresh_viewed();
        // A viewed file folds out of the way; unmarked, it opens again.
        self.set_folded(file, viewed);
        let count = self.loaded.diff.files().len();
        let seen = self.viewed.len();
        self.toast(if viewed {
            format!("Viewed · {seen} of {count} files")
        } else {
            format!("Not viewed · {seen} of {count} files")
        });
    }

    fn open_verdict(&mut self) {
        self.dialog = Dialog::Verdict;
    }

    fn verdict_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('a') => self.send_review(Verdict::Approve),
            KeyCode::Char('n') => self.send_review(Verdict::ApproveWithNotes),
            KeyCode::Char('c') | KeyCode::Char('r') => self.send_review(Verdict::RequestChanges),
            KeyCode::Esc | KeyCode::Char('q') => self.dialog = Dialog::None,
            _ => {}
        }
    }

    fn review_subject(&self) -> String {
        self.loaded.label.clone()
    }

    fn send_review(&mut self, asked: Verdict) {
        self.dialog = Dialog::None;
        let review = diffreview::build(
            &self.loaded.diff,
            &self.review_subject(),
            &self.notes,
            &self.comments,
        );
        if asked == Verdict::RequestChanges && !review.has_notes() {
            self.fail("Add a note first (C on lines, F on a file, M for the whole change), so the agent knows what to change.");
            return;
        }
        let verdict = review.verdict(asked);
        let text = review.render(asked);
        match self.sends {
            Sends::Return => {
                self.outcome = Some(Outcome::Sent { verdict, text });
                self.mark_sent();
                self.quit = true;
            }
            Sends::Agent => match (self.deliver)(&text) {
                Ok(agent) => {
                    self.mark_sent();
                    self.toast(format!(
                        "Review ({}) sent to {}",
                        verdict.label(),
                        agent.describe()
                    ));
                }
                Err(e) => {
                    self.copy = Some(text);
                    self.fail(e.to_string());
                }
            },
        }
    }

    /// The draft comments and the notes went out with the review.
    fn mark_sent(&mut self) {
        let drafts: Vec<usize> = self
            .comments
            .iter()
            .enumerate()
            .filter(|(_, saved)| saved.comment.state == State::Draft)
            .map(|(index, _)| index)
            .collect();
        for index in drafts {
            self.set_state(index, State::Agent);
        }
        self.notes.sent();
        self.persist_notes();
    }

    fn draft_comments_in(&self, path: &str) -> usize {
        self.comments
            .iter()
            .filter(|saved| {
                saved.comment.state == State::Draft && saved.comment.anchor.file == path
            })
            .count()
    }

    fn draw_overview(&mut self, f: &mut Frame, area: Rect, selected: usize) {
        if area.height == 0 {
            return;
        }
        let diff = &self.loaded.diff;
        let files = diff.files();
        let (added, removed) = totals(diff);
        let width = area.width as usize;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let title = format!(" Review · {}", self.review_subject());
        let stats = format!("{} files  +{added} −{removed} ", files.len());
        let pad = width.saturating_sub(cells(&title) + cells(&stats));
        lines.push(Line::from(vec![
            Span::styled(
                clip(&title, width.saturating_sub(cells(&stats))),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(stats, Style::default().fg(DIM)),
        ]));
        lines.push(Line::from(""));
        let pick = |row: usize| {
            if row == selected {
                Style::default().bg(self.palette.focus)
            } else {
                Style::default()
            }
        };
        let global = match &self.notes.global {
            Some(note) => format!(
                "   Whole change  {}",
                note.lines().next().unwrap_or_default()
            ),
            None if selected == 0 => "   Whole change · c adds a note on all of it".to_owned(),
            None => "   Whole change".to_owned(),
        };
        let global_style = if self.notes.global.is_some() {
            pick(0).fg(ACCENT)
        } else {
            pick(0).fg(DIM)
        };
        lines.push(Line::from(Span::styled(
            format!("{:<width$}", clip(&global, width)),
            global_style,
        )));
        lines.push(Line::from(""));
        let path_width = files
            .iter()
            .map(|file| cells(&file.display()) + 1)
            .max()
            .unwrap_or(0)
            .min(width.saturating_sub(34).max(12));
        // Rows for files, scrolled to keep the picked one on screen.
        let room = (area.height as usize).saturating_sub(lines.len());
        let first = selected
            .saturating_sub(1)
            .saturating_sub(room.saturating_sub(1))
            .min(files.len().saturating_sub(room));
        for (index, file) in files.iter().enumerate().skip(first).take(room) {
            let row = index + 1;
            let viewed = self.viewed.contains(&index);
            let old = file.old_path.as_deref().filter(|old| *old != file.path());
            let has_note = self.notes.files.contains_key(file.path());
            let comments = self.draft_comments_in(file.path())
                + old.map_or(0, |old| self.draft_comments_in(old));
            let notes = comments + usize::from(has_note);
            let (status, color) = file_status(file);
            let mark = match (viewed, notes > 0) {
                (_, true) => "●",
                (true, false) => "✓",
                (false, false) => " ",
            };
            let notes_label = match notes {
                0 => String::new(),
                1 => "1 note".to_owned(),
                n => format!("{n} notes"),
            };
            let style = pick(row);
            let mut spans = vec![
                Span::styled(
                    format!(" {mark} "),
                    style.fg(if notes > 0 { ACCENT } else { DIM }),
                ),
                Span::styled(format!("{status} "), style.fg(color)),
                Span::styled(
                    format!("{:<path_width$}", clip(&file.display(), path_width)),
                    if viewed { style.fg(DIM) } else { style },
                ),
                Span::styled(
                    format!("  {:>6}", format!("+{}", file.added)),
                    style.fg(ADDED_FG),
                ),
                Span::styled(
                    format!(" {:>6}", format!("−{}", file.removed)),
                    style.fg(REMOVED_FG),
                ),
                Span::styled(format!("  {notes_label:<9}"), style.fg(ACCENT)),
            ];
            let used: usize = spans.iter().map(|span| cells(&span.content)).sum();
            spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
            lines.push(Line::from(spans));
        }
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_note_editor(&self, f: &mut Frame, area: Rect) {
        let Some(editor) = &self.note_editor else {
            return;
        };
        let title = match &editor.target {
            NoteTarget::Global => " Global note on the whole change ".to_owned(),
            NoteTarget::File(path) => format!(" Note on {path} "),
        };
        let inner_width = area.width.saturating_sub(4).max(1) as usize;
        let text = format!("{}▏", editor.text);
        let lines = wrap_text(&text, inner_width);
        let height = (lines.len() as u16 + 2)
            .clamp(3, (area.height / 2 + 1).max(3))
            .min(area.height);
        let rect = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(height),
            width: area.width,
            height,
        };
        f.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_style(Style::default().fg(ACCENT))
            .title(Line::from(title).style(Style::default().fg(ACCENT)));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        let skip = lines.len().saturating_sub(inner.height as usize);
        let shown: Vec<Line> = lines.into_iter().skip(skip).map(Line::from).collect();
        f.render_widget(Paragraph::new(shown), inner);
    }

    fn draw_verdict(&self, f: &mut Frame, area: Rect) {
        let unsent = self.unsent();
        let notes = match unsent {
            0 => "no notes".to_owned(),
            1 => "1 note".to_owned(),
            n => format!("{n} notes"),
        };
        let to = match self.sends {
            Sends::Return => "the agent that asked",
            Sends::Agent => "the agent pane",
        };
        let lines = vec![
            Line::from(Span::styled(
                format!("Send {notes} to {to}"),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(" a ", Style::default().fg(ACCENT)),
                Span::raw(if unsent > 0 {
                    "approve, with these notes"
                } else {
                    "approve"
                }),
            ]),
            Line::from(vec![
                Span::styled(" n ", Style::default().fg(ACCENT)),
                Span::raw("approve with notes"),
            ]),
            Line::from(vec![
                Span::styled(" c ", Style::default().fg(ACCENT)),
                Span::raw("request changes"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "esc keeps reviewing",
                Style::default().fg(DIM),
            )),
        ];
        panel(f, area, self.look.ink(), "send review", lines, 40);
    }
}

// PART4

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_falls_back_to_fixed_shades_when_the_terminal_wont_say() {
        assert_eq!(Palette::of_terminal(None, true), Palette::light());
        assert_eq!(Palette::of_terminal(None, false), Palette::dark());
    }

    #[test]
    fn the_palette_is_mixed_from_the_themes_own_colors() {
        let theme = Theme {
            background: [0x16, 0x16, 0x16],
            red: [0xd9, 0x6a, 0x4d],
            green: [0x9a, 0xab, 0x84],
            blue: [0x8a, 0xa0, 0xb4],
            accent: [0xe0, 0x5d, 0x3a],
            magenta: [0xb1, 0x93, 0xa8],
        };
        let palette = Palette::of_terminal(Some(theme), true);
        assert_eq!(palette, Palette::from_theme(theme));
        // A fifth of the theme's green over its background; the changed words
        // twice as strong, so they stand out within the line.
        assert_eq!(palette.added, Color::Rgb(0x30, 0x34, 0x2c));
        assert_eq!(palette.added_word, Color::Rgb(0x4b, 0x52, 0x42));
        assert_eq!(palette.removed, Color::Rgb(0x3d, 0x27, 0x21));
        let Color::Rgb(r, g, b) = palette.focus else {
            panic!("the focus tint is exact RGB, got {:?}", palette.focus);
        };
        assert!(b > r && b > g, "the focus row leans blue: {r} {g} {b}");
    }
    use crate::diff::Origin;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    const PATCH: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 }
";

    const TWO_FILES: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
-old
+new
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1,1 +1,1 @@
-one
+two
";

    fn loaded(patch: &str) -> Loaded {
        Loaded {
            diff: Diff::parse(patch.to_owned()),
            title: "leaf diff · test".to_owned(),
            key: "diff:test".to_owned(),
            label: "test".to_owned(),
            request: Request {
                origin: Origin::Patch {
                    path: PathBuf::from("/tmp/test.diff"),
                },
                repo: None,
                paths: Vec::new(),
            },
            github: None,
            branch: Some("main".to_owned()),
        }
    }

    const SHIFTED: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,3 +1,4 @@
 fn main() {
-    let total = sum(items);
+
+    let total = sum(items) + tax;
 }
";

    #[test]
    fn a_line_inserted_above_an_edit_pairs_the_edit_with_its_old_line() {
        let diff = Diff::parse(SHIFTED.to_owned());
        let split = Split::new(&diff, Layout::Split, &HashSet::new());
        let rows: Vec<(Option<&str>, Option<&str>)> = split
            .lines()
            .iter()
            .filter(|line| line.band.is_none())
            .map(|line| {
                (
                    line.left.map(|row| diff.content(row)),
                    line.right.map(|row| diff.content(row)),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (Some("fn main() {"), Some("fn main() {")),
                (None, Some("")),
                (
                    Some("    let total = sum(items);"),
                    Some("    let total = sum(items) + tax;")
                ),
                (Some("}"), Some("}")),
            ]
        );
    }

    #[test]
    fn a_comment_on_a_shifted_pair_hangs_off_its_own_lines_only() {
        let mut app = App::new(loaded(SHIFTED), Options::default());
        let diff = &app.loaded.diff;
        let edit = app
            .split
            .lines()
            .iter()
            .position(|line| {
                line.right
                    .is_some_and(|row| diff.content(row).ends_with("+ tax;"))
            })
            .unwrap_or(usize::MAX);
        app.cursor = edit;
        app.compose(Purpose::Comment);
        let anchor = app.composer.as_ref().map(|c| c.anchor.clone());
        assert_eq!(
            anchor.map(|a| (a.side, a.start_line, a.end_line)),
            Some((Side::New, 3, 3))
        );
    }

    #[test]
    fn emphasis_marks_the_changed_words_of_the_paired_line_in_both_layouts() {
        for layout in [Layout::Split, Layout::Inline] {
            let mut app = App::new(
                loaded(SHIFTED),
                Options {
                    layout,
                    ..Options::default()
                },
            );
            let rows = app.loaded.diff.rows().to_vec();
            let row = |kind: RowKind, nth: usize| {
                rows.iter()
                    .enumerate()
                    .filter(|(_, r)| r.kind == kind)
                    .nth(nth)
                    .map(|(i, _)| i)
                    .unwrap_or(usize::MAX)
            };
            assert!(app.emphasis(row(RowKind::Removed, 0)).is_empty());
            assert!(app.emphasis(row(RowKind::Added, 0)).is_empty());
            assert_eq!(app.emphasis(row(RowKind::Added, 1)), vec![26..32]);
            app.toggle_layout();
            assert_eq!(app.emphasis(row(RowKind::Added, 1)), vec![26..32]);
        }
    }

    fn app() -> App {
        let mut app = App::new(loaded(PATCH), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
    }

    #[test]
    fn live_diff_refreshes_wait_for_drafts_and_keep_saved_comments() {
        let path =
            std::env::temp_dir().join(format!("leaf-diff-live-{}.patch", std::process::id()));
        let changed = PATCH.replace("let b = 3;", "let b = 30;");
        std::fs::write(&path, PATCH).unwrap();
        let request = Request {
            origin: diff::Origin::Patch { path: path.clone() },
            repo: None,
            paths: Vec::new(),
        };
        let mut app = App::new(request.load().unwrap(), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        std::fs::write(&path, &changed).unwrap();
        app.polled -= Duration::from_secs(2);
        press(&mut app, KeyCode::Char('c'));
        app.poll();
        assert!(app.job.is_none(), "typing defers starting the reload");
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        app.poll();
        assert!(matches!(
            app.job,
            Some(Job::Load {
                background: true,
                ..
            })
        ));
        assert!(matches!(app.dialog, Dialog::None));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "Keep this note");
        app.receive_job();
        assert!(
            app.composer.is_some(),
            "a pending reload leaves the draft alone"
        );
        assert!(app.loaded.diff.same_content(&Diff::parse(PATCH.to_owned())));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        let until = Instant::now() + Duration::from_secs(2);
        while app.job.is_some() && Instant::now() < until {
            app.receive_job();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.job.is_none());
        assert!(app.stale);
        assert!(app.loaded.diff.same_content(&Diff::parse(PATCH.to_owned())));
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.body, "Keep this note");
        assert!(!app.marks.is_empty());
        app.remove(0);
        app.polled -= Duration::from_secs(2);
        app.poll();
        let until = Instant::now() + Duration::from_secs(2);
        while app.job.is_some() && Instant::now() < until {
            app.receive_job();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!app.stale);
        assert!(app.loaded.diff.same_content(&Diff::parse(changed)));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_window_over_the_diff_holds_off_a_reload_under_it() {
        // A sidebar is the live view: the diff goes on being reloaded beside it.
        let mut side = app();
        side.polled -= Duration::from_secs(2);
        press(&mut side, KeyCode::Char('t'));
        side.poll();
        assert!(
            matches!(
                side.job,
                Some(Job::Load {
                    background: true,
                    ..
                })
            ),
            "the sidebar keeps the diff live"
        );
        // A window is a snapshot, and nothing is reloaded under it.
        let mut window = app();
        window.polled -= Duration::from_secs(2);
        press(&mut window, KeyCode::Char('T'));
        window.poll();
        assert!(
            window.job.is_none(),
            "a window over the diff is a snapshot and stays one"
        );
    }

    #[test]
    fn changed_annotated_files_keep_the_reviewed_snapshot() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "Review this exact line");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Esc);
        let changed = PATCH
            .replace("@@ -10,3 +10,4 @@", "@@ -10,3 +10,5 @@")
            .replace(" let a = 1;", "+let inserted = 0;\n let a = 1;");
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&changed))),
            remember: None,
            background: true,
        });
        assert!(app.loaded.diff.same_content(&Diff::parse(PATCH.to_owned())));
        assert_eq!(app.comments[0].comment.body, "Review this exact line");
        let comment = app.comments[0].comment.clone();
        app.comment_to_github(&comment, Some(0));
        assert!(matches!(app.dialog, Dialog::None));
        assert!(app.job.is_none());
    }

    #[test]
    fn background_reload_results_do_not_replace_toasts() {
        let mut app = app();
        app.toast("Keep this message");
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&PATCH.replace("let b = 3;", "let b = 30;")))),
            remember: None,
            background: true,
        });
        assert_eq!(app.toast.as_ref().unwrap().text, "Keep this message");
        app.finish(Done::Load {
            result: Box::new(Err(diff::DiffFailed::Git {
                command: "git diff".into(),
                cause: "busy".into(),
            })),
            remember: None,
            background: true,
        });
        assert_eq!(app.toast.as_ref().unwrap().text, "Keep this message");
    }

    #[test]
    fn review_fix_background_reload_keeps_position_when_remembered_line_disappears() {
        let patch = big_patch(30);
        let changed = patch.replace("@@ -1,30 +1,30 @@", "@@ -101,30 +101,30 @@");
        let mut app = App::new(loaded(&patch), Options::default());
        app.view = Rect::new(0, 1, 120, 10);
        app.cursor = 20;
        app.scroll = 15;
        assert_eq!(app.remember(), Some(("src/big.rs".into(), Side::New, 19)));

        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&changed))),
            remember: None,
            background: true,
        });

        assert_eq!((app.cursor, app.scroll), (20, 15));
    }

    #[test]
    fn review_fix_background_reload_snaps_nearby_when_selected_file_disappears() {
        let mut app = App::new(loaded(&format!("{PATCH}{TWO_FILES}")), Options::default());
        app.view = Rect::new(0, 1, 120, 10);
        app.cursor = 4;
        app.scroll = 2;
        assert_eq!(app.remember(), Some(("src/main.rs".into(), Side::New, 12)));

        app.finish(Done::Load {
            result: Box::new(Ok(loaded(TWO_FILES))),
            remember: None,
            background: true,
        });

        assert_eq!((app.cursor, app.scroll), (5, 3));
        assert_eq!(app.current_file(), Some(1));
    }

    #[test]
    fn review_fix_background_reload_clamps_position_when_diff_shrinks() {
        let mut app = App::new(loaded(&big_patch(30)), Options::default());
        app.view = Rect::new(0, 1, 120, 10);
        app.cursor = 27;
        app.scroll = 23;

        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&big_patch(8)))),
            remember: None,
            background: true,
        });

        assert_eq!((app.cursor, app.scroll), (9, 5));
    }

    #[test]
    fn review_fix_manual_reload_keeps_file_start_fallback_and_recenters() {
        let patch = format!("{TWO_FILES}{}", big_patch(30));
        let changed = patch.replace("@@ -1,30 +1,30 @@", "@@ -101,30 +101,30 @@");
        let mut app = App::new(loaded(&patch), Options::default());
        app.view = Rect::new(0, 1, 120, 10);
        app.cursor = 20;
        app.scroll = 17;

        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&changed))),
            remember: app.remember(),
            background: false,
        });

        assert_eq!((app.cursor, app.scroll), (8, 3));
    }

    #[test]
    fn manual_unchanged_reloads_confirm_without_moving() {
        let mut app = app();
        app.scroll = 2;
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(PATCH))),
            remember: app.remember(),
            background: false,
        });
        assert_eq!(app.scroll, 2);
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|toast| toast.text.contains("unchanged"))
        );
    }

    #[test]
    fn an_unchanged_reload_keeps_the_review_in_place() {
        let mut app = app();
        app.scroll = 2;
        app.left = 4;
        app.selection = Some(app.cursor);
        let cursor = app.cursor;
        let remember = app.remember();
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(PATCH))),
            remember,
            background: true,
        });
        assert_eq!(app.cursor, cursor);
        assert_eq!(app.scroll, 2);
        assert_eq!(app.left, 4);
        assert_eq!(app.selection, Some(cursor));
    }

    #[test]
    fn tab_cycles_from_a_pull_requests_code_tab_to_its_other_tabs() {
        let as_code_tab = || Options {
            tabs: true,
            ..Options::default()
        };
        let mut app = App::new(loaded(PATCH), as_code_tab());
        press(&mut app, KeyCode::Tab);
        assert!(app.quit);
        assert_eq!(app.leave, Leave::Checks, "tab goes on to Checks");

        let mut app = App::new(loaded(PATCH), as_code_tab());
        press(&mut app, KeyCode::BackTab);
        assert!(app.quit);
        assert_eq!(
            app.leave,
            Leave::Comments,
            "shift-tab goes back to Comments"
        );

        let mut app = App::new(loaded(PATCH), as_code_tab());
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(app.leave, Leave::Timeline);

        // Outside a pull request, tab is not a way out.
        let mut app = App::new(loaded(PATCH), Options::default());
        press(&mut app, KeyCode::Tab);
        assert!(!app.quit);
    }

    fn press_ctrl(app: &mut App, code: KeyCode) {
        app.key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
    }

    fn type_text(app: &mut App, text: &str) {
        for ch in text.chars() {
            press(app, KeyCode::Char(ch));
        }
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        let app = &mut *app;
        terminal.draw(|f| app.draw(f)).expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_cursor_line_is_washed_end_to_end_and_marked_at_its_edge() {
        for layout in [Layout::Split, Layout::Inline] {
            let mut app = App::new(
                loaded(PATCH),
                Options {
                    layout,
                    ..Options::default()
                },
            );
            let (cursor, added, removed) = (
                app.palette.cursor,
                app.palette.added_cursor,
                app.palette.removed_cursor,
            );
            let line = app.split_content(app.cursor, 80);
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert_eq!(width, 80, "{layout:?}");
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(cursor)),
                "{layout:?}: every cell of an unchanged cursor line is washed"
            );
            assert!(
                line.spans
                    .iter()
                    .any(|s| s.content == "▌" && s.style.fg == Some(ACCENT)),
                "{layout:?}"
            );

            press(&mut app, KeyCode::Char('j'));
            let line = app.split_content(app.cursor, 80);
            assert!(
                line.spans
                    .iter()
                    .any(|s| s.style.bg == Some(added) || s.style.bg == Some(removed)),
                "{layout:?}: a changed cursor line keeps its color, stronger"
            );
            let other = app.split_content(app.cursor + 1, 80);
            assert!(
                other
                    .spans
                    .iter()
                    .all(|s| s.style.bg != Some(cursor) && s.style.fg != Some(ACCENT)),
                "{layout:?}: other lines are left alone"
            );
        }
    }

    fn big_patch(lines: usize) -> String {
        let mut patch = format!(
            "diff --git a/src/big.rs b/src/big.rs\n--- a/src/big.rs\n+++ b/src/big.rs\n@@ -1,{lines} +1,{lines} @@\n"
        );
        for line in 1..=lines {
            patch.push_str(&format!(" line {line}\n"));
        }
        patch
    }

    #[test]
    fn the_view_is_a_split_diff_with_one_footer() {
        let mut app = app();
        let screen = draw(&mut app, 100, 16);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(!screen.contains("leaf diff"));
        assert!(!screen.contains("j/k move"));
        assert!(screen.contains("src/main.rs +2 −1"));
        let paired = lines[3];
        let old = paired.find("let b = 2;").expect("the old line");
        let new = paired.find("let b = 3;").expect("the new line");
        assert!(old < new, "old code stays on the left: {paired}");
        assert!(lines[4].contains("let c = 4;"));
        assert!(lines[15].contains("src/main.rs:10"));
        assert!(lines[15].ends_with("+2 −1"));
    }

    #[test]
    fn the_file_picker_lists_files_and_opens_one() {
        let mut app = App::new(loaded(TWO_FILES), Options::default());
        let screen = draw(&mut app, 100, 16);
        assert!(!screen.contains("▸ a.rs"));
        press(&mut app, KeyCode::Char('f'));
        assert!(matches!(app.dialog, Dialog::Files { selected: 0, .. }));
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("files"));
        assert!(screen.contains("filter changed files"));
        assert!(screen.contains("▸ M a.rs"));
        assert!(screen.contains("  M b.rs"));
        assert!(screen.contains("1 of 2"));

        press(&mut app, KeyCode::Down);
        assert!(matches!(app.dialog, Dialog::Files { selected: 1, .. }));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog, Dialog::None));
        assert_eq!(app.cursor, 5);
        assert_eq!(app.loaded.diff.file_index(app.cursor), Some(1));

        press(&mut app, KeyCode::Char('F'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.dialog, Dialog::None));
        assert!(!app.quit);
    }

    #[test]
    fn the_file_picker_searches_the_changed_files() {
        let mut app = App::new(loaded(TWO_FILES), Options::default());
        press(&mut app, KeyCode::Char('f'));
        type_text(&mut app, "b.");
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("▸ M b.rs"), "{screen}");
        assert!(!screen.contains("M a.rs"), "{screen}");
        assert!(screen.contains("1 of 1 · 2 total"), "{screen}");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.loaded.diff.file_index(app.cursor), Some(1));

        // A search with no matches keeps the picker open and enter does nothing.
        press(&mut app, KeyCode::Char('f'));
        type_text(&mut app, "zzz");
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("no file matches"), "{screen}");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog, Dialog::Files { .. }));

        // Escape empties the search first, then closes the picker.
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.dialog, Dialog::Files { .. }));
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("filter changed files"), "{screen}");
        assert!(screen.contains("a.rs"), "{screen}");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.dialog, Dialog::None));
        assert!(!app.quit);
    }

    #[test]
    fn the_composer_is_a_rule_above_the_footer() {
        let mut app = app();
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "why?");
        let screen = draw(&mut app, 100, 16);
        let lines: Vec<&str> = screen.lines().collect();
        assert!(lines[15].contains("enter comment"));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("src/main.rs L10") && line.contains('─'))
        );
        assert!(screen.contains("why?"));
    }

    #[test]
    fn jumping_to_a_range_scrolls_the_whole_block_into_view() {
        let mut app = App::new(loaded(&big_patch(400)), Options::default());
        app.view = Rect::new(0, 1, 120, 10);
        app.scroll = 380;
        app.jump_to(&Anchor {
            file: "src/big.rs".to_owned(),
            side: Side::New,
            start_line: 250,
            end_line: 252,
        });
        assert_eq!(app.focus, Some((251, 253)));
        assert!(app.scroll <= 251);
        assert!(253 < app.scroll + 10);
        let screen = draw(&mut app, 100, 12);
        assert!(screen.contains("line 250"));
        assert!(screen.contains("line 252"));
    }

    #[test]
    fn visual_mode_stops_extending_on_escape() {
        let mut app = app();
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        assert!(app.visual);
        assert_eq!(app.range(), (3, 4));
        press(&mut app, KeyCode::Esc);
        assert!(!app.quit);
        assert!(!app.visual);
        assert!(app.selection.is_none(), "escape drops the highlight");
        assert!(app.focus.is_none());

        // Escape only quits once nothing is highlighted any more.
        press(&mut app, KeyCode::Esc);
        assert!(app.quit);
    }

    #[test]
    fn a_visual_range_comments_on_enter() {
        let mut app = app();
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('C'));

        let composer = app.composer.as_ref().expect("a comment box");
        assert_eq!(composer.anchor.file, "src/main.rs");
        assert_eq!(composer.anchor.side, Side::New);
        assert_eq!(
            (composer.anchor.start_line, composer.anchor.end_line),
            (11, 12)
        );
        assert_eq!((composer.first, composer.last), (3, 5));

        type_text(&mut app, "why?");
        press(&mut app, KeyCode::Enter);
        assert!(app.composer.is_none());
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.body, "why?");
        assert_eq!(app.comments[0].comment.state, State::Draft);
    }

    #[test]
    fn ctrl_enter_pastes_into_the_agent() {
        let sent = Rc::new(RefCell::new(None));
        let mut app = app();
        let captured = sent.clone();
        app.deliver = Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(agent::Agent {
                pane: "%3".to_owned(),
                command: "claude".to_owned(),
            })
        });
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "why?");
        press_ctrl(&mut app, KeyCode::Enter);
        assert!(app.composer.is_none());
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.state, State::Agent);

        let text = sent.borrow().clone().expect("the payload");
        assert!(text.contains("> src/main.rs L11"));
        assert!(text.contains("fn main()"));
        assert!(text.contains("-let b = 2;\n+let b = 3;"));
        assert!(text.contains("let a = 1;"));
        assert!(text.ends_with("why?\n"));
    }

    #[test]
    fn at_asks_the_agent_about_the_line_with_enter() {
        let sent = Rc::new(RefCell::new(None));
        let mut app = app();
        let captured = sent.clone();
        app.deliver = Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(agent::Agent {
                pane: "%3".to_owned(),
                command: "claude".to_owned(),
            })
        });
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('@'));
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("ask the agent"), "{screen}");
        type_text(&mut app, "is this safe?");
        press(&mut app, KeyCode::Enter);
        assert!(app.composer.is_none());
        let text = sent.borrow().clone().expect("the payload");
        assert!(text.contains("> src/main.rs L11"));
        assert!(text.ends_with("is this safe?\n"));
        // ctrl+enter outside the box opens the same question box.
        press_ctrl(&mut app, KeyCode::Enter);
        assert_eq!(app.composer.as_ref().map(|c| c.purpose), Some(Purpose::Ask));
    }

    #[test]
    fn a_question_from_the_rex_palette_is_sent_about_the_line() {
        let sent = Rc::new(RefCell::new(None));
        let mut app = app();
        let captured = sent.clone();
        app.deliver = Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(agent::Agent {
                pane: "%3".to_owned(),
                command: "claude".to_owned(),
            })
        });
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, remote::WAKE);
        type_text(&mut app, "ask why this change?");
        press(&mut app, KeyCode::Enter);
        let text = sent.borrow().clone().expect("the payload");
        assert!(text.contains("> src/main.rs L11"));
        assert!(text.ends_with("why this change?\n"));
        // Without a question it opens the box to type one.
        press(&mut app, remote::WAKE);
        type_text(&mut app, "ask");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.composer.as_ref().map(|c| c.purpose), Some(Purpose::Ask));
    }

    #[test]
    fn failed_agent_delivery_keeps_a_draft_and_copies_the_prompt() {
        let mut app = app();
        app.deliver = Box::new(|_| Err(agent::AgentFailed::NoTmux));
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "check this");
        press_ctrl(&mut app, KeyCode::Enter);

        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.state, State::Draft);
        assert!(app.comments.is_empty() || app.comments[0].comment.body == "check this");
        let copied = app.copy.clone().expect("clipboard fallback");
        assert!(copied.contains("> src/main.rs L10"));
        assert!(copied.contains(" let a = 1;"));
        assert!(app.toast.as_ref().is_some_and(|toast| toast.error));
    }

    #[test]
    fn escape_cancels_the_comment() {
        let mut app = app();
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "later");
        press(&mut app, KeyCode::Esc);
        assert!(app.composer.is_none());
        assert!(app.comments.is_empty());
        assert!(!app.quit);
    }

    #[test]
    fn github_posting_asks_first_and_returns_to_editing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "post me");
        press_ctrl(&mut app, KeyCode::Char('g'));
        assert!(matches!(app.dialog, Dialog::Confirm(_)));
        press(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.dialog, Dialog::None));
        assert_eq!(
            app.composer.as_ref().map(Composer::body).as_deref(),
            Some("post me")
        );
    }

    #[test]
    fn github_success_closes_posting_dialog() {
        let mut app = app();
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "post me");
        press_ctrl(&mut app, KeyCode::Char('g'));
        let Dialog::Confirm(confirm) = std::mem::replace(
            &mut app.dialog,
            Dialog::Loading("Posting to GitHub…".to_owned()),
        ) else {
            panic!("posting must ask for confirmation first");
        };
        let index = app.add_comment(Comment {
            anchor: confirm.anchor,
            body: confirm.body,
            state: State::Draft,
        });
        // Substitute only the network worker's response; use the real receiver
        // and completion handler that the event loop calls.
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job::Post {
            rx,
            index,
            composer: confirm.composer,
            added: true,
        });
        tx.send(Ok("owner/repo#12".to_owned())).unwrap();
        app.receive_job();

        assert!(app.job.is_none(), "the successful response was received");
        assert_eq!(app.comments[index].comment.state, State::Github);
        assert_eq!(
            app.toast.as_ref().unwrap().text,
            "Comment posted to owner/repo#12"
        );
        assert!(
            matches!(app.dialog, Dialog::None),
            "the Posting to GitHub dialog must close after success"
        );
    }

    #[test]
    fn copying_a_range_carries_its_anchor_and_patch() {
        let mut app = app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('y'));
        let copied = app.copy.clone().expect("copied");
        assert!(copied.starts_with(
            "> src/main.rs L11-L12 (new side). The diff includes the lines around that range:\n\n```diff\n"
        ));
        assert!(copied.contains("-let b = 2;\n+let b = 3;\n+let c = 4;"));
        assert!(!copied.contains("why?"));
    }

    #[test]
    fn jumping_to_a_comment_focuses_its_whole_range() {
        let mut app = app();
        app.add_comment(Comment {
            anchor: Anchor {
                file: "src/main.rs".to_owned(),
                side: Side::New,
                start_line: 11,
                end_line: 12,
            },
            body: "both".to_owned(),
            state: State::Draft,
        });
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.dialog, Dialog::None));
        assert_eq!(app.selection, Some(3));
        assert_eq!(app.cursor, 3);
        assert_eq!(app.focus, Some((3, 4)));
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|t| t.text == "src/main.rs L11-L12")
        );

        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.cursor, 4);
        assert!(app.focus.is_none());
        assert!(app.selection.is_none());
    }

    #[test]
    fn the_comments_panel_jumps_and_deletes() {
        let mut app = app();
        let comment = Comment {
            anchor: Anchor {
                file: "src/main.rs".to_owned(),
                side: Side::New,
                start_line: 12,
                end_line: 12,
            },
            body: "first".to_owned(),
            state: State::Draft,
        };
        app.add_comment(comment.clone());
        app.add_comment(Comment {
            body: "second".to_owned(),
            ..comment
        });
        press(&mut app, KeyCode::Char('m'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.body, "first");
        assert!(matches!(app.dialog, Dialog::Comments { selected: 0 }));
    }

    #[test]
    fn the_view_stays_near_the_cursor_in_a_huge_diff() {
        let mut app = App::new(loaded(&big_patch(5000)), Options::default());
        app.view = Rect::new(0, 1, 120, 12);
        app.cursor = 4000;
        app.follow_cursor();
        assert!(app.scroll <= app.cursor);
        assert!(app.cursor < app.scroll + 12);
        let screen = draw(&mut app, 100, 16);
        assert!(screen.contains("big.rs"));
        assert!(app.highlight.len() <= 1);
    }

    #[test]
    fn scrolling_back_up_keeps_the_first_files_name_on_screen() {
        let mut app = App::new(loaded(&big_patch(80)), Options::default());
        app.view = Rect::new(0, 1, 120, 12);
        press(&mut app, KeyCode::PageDown);
        assert!(app.scroll > 0);
        for _ in 0..20 {
            press(&mut app, KeyCode::Char('u'));
        }
        assert_eq!(app.scroll, 0, "the file heading scrolls back into view");
        assert_eq!(app.cursor, app.split.first_content().unwrap());

        // Plain d and u are the half-page keys the help has always shown.
        let first = app.cursor;
        press(&mut app, KeyCode::Char('d'));
        assert!(app.cursor > first);
        for _ in 0..20 {
            press(&mut app, KeyCode::Char('u'));
        }
        assert_eq!(app.scroll, 0, "the file heading scrolls back into view");
        assert_eq!(app.cursor, first);
        let screen = draw(&mut app, 100, 22);
        assert!(screen.contains("big.rs"));
    }

    #[test]
    fn the_wheel_scrolls_back_to_the_first_files_name() {
        let mut app = App::new(loaded(&big_patch(80)), Options::default());
        app.view = Rect::new(0, 1, 120, 12);
        for _ in 0..10 {
            wheel(&mut app, MouseEventKind::ScrollDown);
        }
        assert!(app.scroll > 0);
        for _ in 0..80 {
            wheel(&mut app, MouseEventKind::ScrollUp);
            // Each frame follows the cursor, which must not pull the view back
            // down onto the cursor while the wheel is still going up.
            app.follow_cursor();
        }
        assert_eq!(app.scroll, 0, "the file heading scrolls back into view");
        let screen = draw(&mut app, 100, 22);
        assert!(screen.contains("big.rs"));
    }

    #[test]
    fn the_horizontal_wheel_pans_code_but_not_gutters_or_selection() {
        let patch = PATCH
            .replace("let b = 2;", "000-old-TAIL")
            .replace("let b = 3;", "000-new-TAIL");
        for (layout, width, panned) in [
            (Layout::Split, 27, "▌ 11 old-TAIL│▌ 11 new-TAIL"),
            (Layout::Inline, 17, "▌ 11     old-TAIL\n▌     11 new-TAIL"),
        ] {
            let mut app = App::new(
                loaded(&patch),
                Options {
                    layout,
                    ..Options::default()
                },
            );
            app.scroll = 2;
            app.cursor = 3;
            app.selection = Some(2);
            let before = draw(&mut app, width, 5);
            assert!(!before.contains("TAIL"));

            wheel(&mut app, MouseEventKind::ScrollRight);
            let after = draw(&mut app, width, 5);
            assert!(after.contains(panned), "{layout:?}:\n{after}");
            assert_eq!((app.scroll, app.cursor, app.selection), (2, 3, Some(2)));

            wheel(&mut app, MouseEventKind::ScrollLeft);
            assert_eq!(draw(&mut app, width, 5), before);
            wheel(&mut app, MouseEventKind::ScrollLeft);
            assert_eq!(app.left, 0);
        }
    }

    #[test]
    fn horizontal_scroll_stops_at_the_last_content_column() {
        // 14 display columns: the tab after "ab" occupies two, as does 界.
        let patch = PATCH.replace("let b = 3;", "ab\t界12345END");
        for (layout, width, limit) in [(Layout::Split, 26, 6), (Layout::Inline, 18, 5)] {
            let mut app = App::new(
                loaded(&patch),
                Options {
                    layout,
                    ..Options::default()
                },
            );
            draw(&mut app, width, 12);
            for _ in 0..20 {
                wheel(&mut app, MouseEventKind::ScrollRight);
            }
            assert_eq!(app.left, limit, "{layout:?}");
            assert!(draw(&mut app, width, 12).contains("END"));
            wheel(&mut app, MouseEventKind::ScrollLeft);
            assert_eq!(app.left, limit - 4);
            draw(&mut app, 80, 12);
            assert_eq!(app.left, 0, "all lines fit after resize");
            press(&mut app, KeyCode::Char('L'));
            assert_eq!(app.left, 0, "keys share the wheel limit");
        }
    }

    #[test]
    fn horizontal_scroll_clips_in_terminal_cells() {
        for (text, room, offset, expected) in
            [("\tabc", 4, 3, " abc"), ("a#\u{fe0f}X", 3, 1, "#\u{fe0f}X")]
        {
            let start = cells(text).saturating_sub(room);
            let mut spans = Vec::new();
            text_spans(
                &mut spans,
                text,
                start,
                room,
                Style::default(),
                None,
                &[],
                Color::Reset,
                None,
            );
            assert_eq!(start, offset);
            assert_eq!(Line::from(spans).to_string(), expected);
        }
    }

    #[test]
    fn horizontal_scroll_reclamps_after_a_layout_switch_or_diff_refresh() {
        let patch = PATCH.replace("let b = 2;", "123456789abcde");
        let mut app = App::new(loaded(&patch), Options::default());
        draw(&mut app, 26, 12);
        for _ in 0..4 {
            wheel(&mut app, MouseEventKind::ScrollRight);
        }
        assert_eq!(app.left, 7, "the old side has seven columns for code");
        press(&mut app, KeyCode::Char('i'));
        draw(&mut app, 26, 12);
        assert_eq!(app.left, 0, "the whole line fits inline");
        press(&mut app, KeyCode::Char('i'));
        wheel(&mut app, MouseEventKind::ScrollRight);
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(TWO_FILES))),
            remember: None,
            background: true,
        });
        draw(&mut app, 26, 12);
        assert_eq!(
            app.left, 0,
            "the replacement diff contains only short lines"
        );
    }

    #[test]
    fn the_horizontal_wheel_does_not_pan_behind_overlays_or_inputs() {
        for key in [
            KeyCode::Char('?'),
            KeyCode::Char('c'),
            KeyCode::Char('O'),
            KeyCode::Char('M'),
            remote::WAKE,
        ] {
            let mut app = app();
            draw(&mut app, 19, 12);
            wheel(&mut app, MouseEventKind::ScrollRight);
            assert!(app.left > 0);
            press(&mut app, KeyCode::Char('0'));
            press(&mut app, key);
            wheel(&mut app, MouseEventKind::ScrollRight);
            assert_eq!(app.left, 0, "after {key:?}");
        }
    }

    #[test]
    fn the_outline_window_keeps_the_horizontal_wheel_off_the_diff() {
        let mut app = app();
        draw(&mut app, 19, 12);
        wheel(&mut app, MouseEventKind::ScrollRight);
        assert!(app.left > 0);
        press(&mut app, KeyCode::Char('0'));
        press(&mut app, KeyCode::Char('T'));
        wheel(&mut app, MouseEventKind::ScrollRight);
        assert_eq!(app.left, 0, "the window covers the diff");
    }

    #[test]
    fn a_files_name_stays_pinned_while_its_lines_scroll() {
        let mut app = App::new(loaded(&big_patch(80)), Options::default());
        app.cursor = 8;
        app.scroll = 6;
        let screen = draw(&mut app, 100, 22);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(
            rows[0].contains("big.rs"),
            "the file name heads the view even with its own row scrolled off:\n{screen}"
        );
        assert!(!rows[1].contains("big.rs"), "the heading is pinned once");

        app.scroll = 0;
        app.cursor = 2;
        let screen = draw(&mut app, 100, 22);
        let rows: Vec<&str> = screen.lines().collect();
        assert!(rows[0].contains("big.rs"));
        assert!(!rows[1].contains("big.rs"), "no duplicate heading");
    }

    #[test]
    fn a_click_under_the_pinned_name_lands_on_the_line_shown_there() {
        let mut app = App::new(loaded(&big_patch(80)), Options::default());
        app.view = Rect::new(0, 0, 120, 22);
        app.cursor = 8;
        app.scroll = 6;
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(
            app.cursor, 6,
            "the row under the pinned name is the top line"
        );
        assert!(app.selection.is_none());
    }

    fn wheel(app: &mut App, kind: MouseEventKind) {
        app.mouse(MouseEvent {
            kind,
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn keys_skip_file_and_hunk_headings() {
        let mut app = app();
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.cursor, 3);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.cursor, 5);
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn a_range_of_only_removed_lines_anchors_to_the_old_side() {
        let patch = "\
diff --git a/x b/x
--- a/x
+++ b/x
@@ -1,3 +1,2 @@
 one
-two
-three
+four
";
        let mut app = App::new(loaded(patch), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('C'));
        let composer = app.composer.as_ref().expect("a comment box");
        assert_eq!(composer.anchor.side, Side::Old);
        assert_eq!(composer.anchor.start_line, 3);
    }

    #[test]
    fn the_payload_quotes_the_range_and_fences_the_patch() {
        let diff = Diff::parse(PATCH.to_owned());
        let anchor = Anchor {
            file: "src/main.rs".to_owned(),
            side: Side::New,
            start_line: 11,
            end_line: 12,
        };
        let text = payload(&diff, &anchor, 4, 5, "both new");
        assert_eq!(
            text,
            "> src/main.rs L11-L12 (new side). The diff includes the lines around that range:\n\n```diff\n@@ -10,3 +10,4 @@ fn main() {\n let a = 1;\n-let b = 2;\n+let b = 3;\n+let c = 4;\n }\n```\n\nboth new\n"
        );
    }

    #[test]
    fn i_switches_to_one_column_with_removed_lines_above_added_ones() {
        let mut app = app();
        press(&mut app, KeyCode::Char('j'));
        let row = app.current_row();
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.current_row(), row, "the cursor stays on its line");
        let screen = draw(&mut app, 100, 16);
        let lines: Vec<&str> = screen.lines().collect();
        let removed = lines.iter().position(|line| line.contains("let b = 2;"));
        let added = lines.iter().position(|line| line.contains("let b = 3;"));
        assert!(
            removed.is_some_and(|removed| added.is_some_and(|added| removed < added)),
            "removed above added, each on its own line:\n{screen}"
        );
        let context = lines
            .iter()
            .find(|line| line.contains("let a = 1;"))
            .expect("the context line");
        assert!(context.contains("10") && !context.contains('│'));
        assert_eq!(context.matches("let a = 1;").count(), 1, "drawn once");

        press(&mut app, KeyCode::Char('i'));
        let screen = draw(&mut app, 100, 16);
        assert!(
            screen
                .lines()
                .any(|line| line.contains("let b = 2;") && line.contains("let b = 3;"))
        );
    }

    #[test]
    fn switching_the_layout_remembers_it() {
        let store = Store::open(std::path::Path::new(":memory:")).expect("in-memory store");
        let mut app = App::new(
            loaded(PATCH),
            Options {
                store: Some(store),
                ..Options::default()
            },
        );
        let saved = |app: &App| {
            app.store
                .as_ref()
                .and_then(|store| store.load_diff_layout().expect("reads"))
        };
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(saved(&app), Some(Layout::Inline));
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(saved(&app), Some(Layout::Split));
        assert!(app.toast.is_none());
    }

    #[test]
    fn switching_the_layout_without_a_store_says_it_will_not_last() {
        let mut app = App::new(loaded(PATCH), Options::default());
        press(&mut app, KeyCode::Char('i'));
        let toast = app.toast.as_ref().expect("a toast");
        assert!(
            toast
                .text
                .starts_with("This layout lasts only until you quit")
        );
    }

    #[test]
    fn a_comment_on_an_inline_removed_line_anchors_to_the_old_side() {
        let mut app = App::new(
            loaded(PATCH),
            Options {
                layout: Layout::Inline,
                ..Options::default()
            },
        );
        app.view = Rect::new(0, 1, 120, 20);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('C'));
        let composer = app.composer.as_ref().expect("a comment box");
        assert_eq!(composer.anchor.side, Side::Old);
        assert_eq!(composer.anchor.start_line, 11);
    }

    #[test]
    fn a_diff_without_changes_says_so() {
        let mut app = App::new(loaded(""), Options::default());
        let screen = draw(&mut app, 80, 10);
        assert!(screen.contains("nothing changed"));
    }

    fn reviewing(patch: &str) -> App {
        let options = Options {
            margin: Margin {
                sends: Sends::Return,
                overview_first: true,
                ..Margin::default()
            },
            ..Options::default()
        };
        let mut app = App::new(loaded(patch), options);
        app.view = Rect::new(0, 1, 120, 20);
        app
    }

    fn outcome_text(app: &App) -> String {
        match &app.outcome {
            Some(Outcome::Sent { text, .. }) => text.clone(),
            other => panic!("expected a sent review, got {other:?}"),
        }
    }

    #[test]
    fn a_review_opens_on_an_overview_of_every_file() {
        let mut app = reviewing(TWO_FILES);
        let screen = draw(&mut app, 100, 12);
        assert!(screen.contains("Review · test"), "{screen}");
        assert!(screen.contains("2 files"), "{screen}");
        assert!(screen.contains("Whole change"), "{screen}");
        assert!(
            screen.contains("a.rs") && screen.contains("b.rs"),
            "{screen}"
        );
        assert_eq!(app.overview, Some(1));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.overview, None);
        assert_eq!(app.current_file(), Some(1));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.overview, Some(2));
    }

    #[test]
    fn pasting_goes_into_the_note_editor() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        app.paste("one\r\ntwo");
        assert_eq!(
            app.note_editor.as_ref().map(|e| e.text.as_str()),
            Some("one\ntwo")
        );
    }

    #[test]
    fn alt_backspace_over_a_wide_space_keeps_the_text_whole() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        app.paste("foo\u{3000}bar");
        app.key(KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::ALT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert_eq!(
            app.note_editor.as_ref().map(|e| e.text.as_str()),
            Some("foo\u{3000}")
        );
    }

    #[test]
    fn ctrl_w_deletes_a_word_in_the_note_editor() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        app.paste("keep this word");
        app.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(
            app.note_editor.as_ref().map(|e| e.text.as_str()),
            Some("keep this ")
        );
    }

    #[test]
    fn the_note_editor_fits_a_short_area() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        type_text(
            &mut app,
            "a note long enough to wrap onto several rows of text",
        );
        for height in [1, 2, 3, 4] {
            let mut terminal = Terminal::new(TestBackend::new(30, height)).expect("test terminal");
            terminal
                .draw(|f| app.draw_note_editor(f, f.area()))
                .expect("draws");
        }
    }

    #[test]
    fn a_note_on_a_renamed_files_old_path_moves_to_its_new_one() {
        let renamed = "\
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1,1 +1,1 @@
-old
+new
";
        let saved = Rc::new(RefCell::new(None));
        let sink = saved.clone();
        let mut app = App::new(
            loaded(renamed),
            Options {
                margin: Margin {
                    draft: Some(r#"{"files":{"old.rs":"keep this"}}"#.into()),
                    save: Box::new(move |json| *sink.borrow_mut() = json.map(str::to_owned)),
                    sends: Sends::Return,
                    overview_first: true,
                    ..Margin::default()
                },
                ..Options::default()
            },
        );
        assert_eq!(
            app.notes.files.get("new.rs").map(String::as_str),
            Some("keep this")
        );
        assert!(
            saved
                .borrow()
                .as_deref()
                .is_some_and(|s| s.contains("new.rs"))
        );
        press(&mut app, KeyCode::Char('x'));
        assert!(app.notes.files.is_empty());
    }

    #[test]
    fn the_last_file_goes_on_to_the_overview_when_reviewing() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.current_file(), Some(1));
        press(&mut app, KeyCode::Char(']'));
        assert!(app.overview.is_some());
    }

    #[test]
    fn line_file_and_global_notes_go_out_as_one_review() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "split it up");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "rename the file");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "why two?");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.unsent(), 3);
        press(&mut app, KeyCode::Char('S'));
        assert!(matches!(app.dialog, Dialog::Verdict));
        press(&mut app, KeyCode::Char('c'));
        assert!(app.quit);
        let text = outcome_text(&app);
        assert!(
            text.starts_with("# Review of test: changes requested"),
            "{text}"
        );
        assert!(text.contains("Global: split it up"), "{text}");
        assert!(
            text.contains("## b.rs\n\n### 1. Whole file\n\nrename the file"),
            "{text}"
        );
        assert!(text.contains("+two"), "{text}");
        assert!(text.contains("why two?"), "{text}");
        assert_eq!(app.comments[0].comment.state, State::Agent);
        assert_eq!(app.notes.count(), 0);
    }

    #[test]
    fn in_a_plain_diff_s_delivers_the_review_to_the_agent_pane() {
        let sent = Rc::new(RefCell::new(None));
        let mut app = app();
        let captured = sent.clone();
        app.deliver = Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(agent::Agent {
                pane: "%3".to_owned(),
                command: "claude".to_owned(),
            })
        });
        press(&mut app, KeyCode::Char('F'));
        type_text(&mut app, "looks fine");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('a'));
        assert!(!app.quit);
        let text = sent.borrow().clone().unwrap_or_default();
        assert!(
            text.starts_with("# Review of test: approved with notes"),
            "{text}"
        );
        assert!(
            text.contains("## src/main.rs\n\n### 1. Whole file\n\nlooks fine"),
            "{text}"
        );
        assert_eq!(app.notes.count(), 0);
    }

    #[test]
    fn requesting_changes_without_notes_is_refused() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        assert!(!app.quit);
        assert_eq!(app.outcome, None);
    }

    #[test]
    fn viewed_files_are_ticked_in_the_overview() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('V'));
        assert!(app.notes.is_viewed(&app.loaded.diff, 0));
        let screen = draw(&mut app, 100, 12);
        assert!(screen.contains(" ✓ M a.rs"), "{screen}");
    }

    fn on_file(app: &App) -> Option<String> {
        app.current_file()
            .map(|file| app.loaded.diff.files()[file].path().to_owned())
    }

    #[test]
    fn h_folds_the_file_to_its_heading_and_l_opens_it() {
        let mut app = App::new(loaded(TWO_FILES), Options::default());
        draw(&mut app, 80, 12);
        let open = app.split.lines().len();
        press(&mut app, KeyCode::Char('h'));
        assert!(app.folded.contains("a.rs"));
        assert!(app.split.lines().len() < open);
        assert!(
            app.split.lines()[app.cursor].folded,
            "cursor rests on the heading"
        );
        assert_eq!(on_file(&app).as_deref(), Some("a.rs"));
        let screen = draw(&mut app, 80, 12);
        assert!(screen.contains("▸ a.rs"), "{screen}");
        assert!(
            !screen.contains("✓ viewed"),
            "folded isn't viewed: {screen}"
        );
        assert!(!screen.contains("old"), "{screen}");
        assert!(screen.contains("two"), "{screen}");
        // j moves from the folded heading into the next file.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(on_file(&app).as_deref(), Some("b.rs"));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(on_file(&app).as_deref(), Some("a.rs"));
        press(&mut app, KeyCode::Char('l'));
        assert!(app.folded.is_empty());
        assert_eq!(app.split.lines().len(), open);
        assert!(
            app.split.lines()[app.cursor].is_content(),
            "cursor on its first line"
        );
        assert_eq!(on_file(&app).as_deref(), Some("a.rs"));
        assert!(draw(&mut app, 80, 12).contains("old"));
    }

    #[test]
    fn marking_a_file_viewed_folds_it_and_unmarking_opens_it() {
        let mut app = App::new(loaded(TWO_FILES), Options::default());
        press(&mut app, KeyCode::Char('w'));
        assert!(app.notes.is_viewed(&app.loaded.diff, 0));
        assert!(app.folded.contains("a.rs"));
        let screen = draw(&mut app, 80, 12);
        assert!(screen.contains("▸ a.rs +1 −1  ✓ viewed"), "{screen}");
        // Opened again, it still says viewed.
        press(&mut app, KeyCode::Char('l'));
        let screen = draw(&mut app, 80, 12);
        assert!(screen.contains("▎ a.rs +1 −1  ✓ viewed"), "{screen}");
        assert!(screen.contains("old"), "{screen}");
        press(&mut app, KeyCode::Char('w'));
        assert!(!app.notes.is_viewed(&app.loaded.diff, 0));
        assert!(app.folded.is_empty());
        assert!(!draw(&mut app, 80, 12).contains("✓ viewed"));
        // From the overview too.
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('w'));
        assert!(app.folded.contains("b.rs"));
    }

    #[test]
    fn viewed_files_open_folded_and_opening_one_from_the_overview_unfolds_it() {
        let mut first = App::new(loaded(TWO_FILES), Options::default());
        press(&mut first, KeyCode::Char('w'));
        let draft = first.notes.to_json();
        let options = Options {
            margin: Margin {
                draft,
                overview_first: true,
                ..Margin::default()
            },
            ..Options::default()
        };
        let mut app = App::new(loaded(TWO_FILES), options);
        assert!(app.folded.contains("a.rs"));
        assert!(app.split.lines()[app.cursor].folded);
        press(&mut app, KeyCode::Enter);
        assert!(app.folded.is_empty());
        assert_eq!(on_file(&app).as_deref(), Some("a.rs"));
    }

    #[test]
    fn a_folded_file_that_changes_on_reload_opens_again() {
        let mut app = App::new(loaded(TWO_FILES), Options::default());
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('h'));
        assert_eq!(app.folded.len(), 2);
        let changed = TWO_FILES.replace("+two", "+three");
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(&changed))),
            remember: None,
            background: true,
        });
        assert!(app.folded.contains("a.rs"));
        assert!(!app.folded.contains("b.rs"));
    }

    #[test]
    fn quitting_a_review_with_unsent_notes_takes_a_second_q() {
        let mut app = reviewing(TWO_FILES);
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "hm");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
        assert_eq!(app.outcome, None);
    }

    #[test]
    fn notes_are_saved_as_they_change_and_come_back() {
        let saved = Rc::new(RefCell::new(None));
        let sink = saved.clone();
        let options = Options {
            margin: Margin {
                save: Box::new(move |json| *sink.borrow_mut() = json.map(str::to_owned)),
                ..Margin::default()
            },
            ..Options::default()
        };
        let mut app = App::new(loaded(PATCH), options);
        press(&mut app, KeyCode::Char('M'));
        type_text(&mut app, "kept");
        press(&mut app, KeyCode::Enter);
        let draft = saved.borrow().clone();
        let again = App::new(
            loaded(PATCH),
            Options {
                margin: Margin {
                    draft,
                    ..Margin::default()
                },
                ..Options::default()
            },
        );
        assert_eq!(again.notes.global.as_deref(), Some("kept"));
    }

    /// Three files under two directories, two of them a directory deep.
    const TREE: &str = "\
diff --git a/src/core/a.rs b/src/core/a.rs
--- a/src/core/a.rs
+++ b/src/core/a.rs
@@ -1,1 +1,2 @@
 one
+two
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,1 @@
-old main
+new main
diff --git a/docs/links.md b/docs/links.md
--- a/docs/links.md
+++ b/docs/links.md
@@ -1,1 +1,1 @@
-old link
+new link
";

    fn tree_app() -> App {
        let mut app = App::new(loaded(TREE), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        app
    }

    /// The outline's own columns of the screen, above the footer.
    fn beside(frame: &str) -> String {
        let rows: Vec<&str> = frame.lines().collect();
        rows[..rows.len().saturating_sub(1)]
            .iter()
            .map(|row| {
                row.chars()
                    .take(OUTLINE_COLUMNS as usize)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The entry the outline marks as the one the cursor is in, as far as the
    /// row goes.
    fn picked(frame: &str) -> String {
        beside(frame)
            .lines()
            .find_map(|row| row.split_once('›').map(|(_, row)| row.to_owned()))
            .unwrap_or_default()
    }

    fn click(app: &mut App, column: u16, row: u16) {
        app.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    #[test]
    fn a_release_over_the_outline_ends_a_drag_begun_in_the_diff() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        click(&mut app, 60, 3);
        assert!(app.drag_origin.is_some(), "the press began a drag");
        app.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 10,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        assert!(
            app.drag_origin.is_none(),
            "the release over the outline did not end the drag"
        );
        assert!(
            app.can_refresh(),
            "the diff waits on a drag that never ends"
        );
    }
    #[test]
    fn the_outline_beside_the_diff_lists_the_files_by_directory() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        let screen = draw(&mut app, 120, 20);
        let side = beside(&screen);
        for entry in ["src/", "core/", "a.rs", "main.rs", "docs/", "links.md"] {
            assert!(side.contains(entry), "{entry} missing:\n{screen}");
        }
        // The diff keeps a column of its own beside it, and still shows.
        assert!(app.view.x >= OUTLINE_COLUMNS);
        assert!(screen.contains("new main"), "{screen}");
        assert!(picked(&screen).contains("a.rs"), "{screen}");
    }

    /// `src` left for `docs` and met again: the directory is one entry.
    #[test]
    fn an_outline_move_opens_that_file() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        // Down the tree: a directory holds no file, so the move goes past it.
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.current_file(), Some(1));
        let screen = draw(&mut app, 120, 20);
        assert!(screen.contains("new main"), "{screen}");
        assert!(picked(&screen).contains("main.rs"), "{screen}");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.current_file(), Some(0));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.current_file(), Some(2));
        assert!(draw(&mut app, 120, 20).contains("new link"));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.current_file(), Some(0));
    }

    #[test]
    fn dragging_the_outline_s_edge_resizes_it_beside_the_diff() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        let drag = |app: &mut App, kind: MouseEventKind, column: u16| {
            app.mouse(MouseEvent {
                kind,
                column,
                row: 3,
                modifiers: KeyModifiers::NONE,
            });
        };
        drag(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            OUTLINE_COLUMNS - 1,
        );
        drag(&mut app, MouseEventKind::Drag(MouseButton::Left), 49);
        drag(&mut app, MouseEventKind::Up(MouseButton::Left), 49);
        draw(&mut app, 120, 20);
        assert_eq!(app.view.x, 50);
        assert!(
            app.drag_origin.is_none(),
            "the edge's drag is no drag in the diff"
        );
    }

    #[test]
    fn angle_brackets_resize_the_outline_beside_the_diff() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        assert_eq!(app.view.x, OUTLINE_COLUMNS);
        press(&mut app, KeyCode::Char('>'));
        draw(&mut app, 120, 20);
        assert_eq!(app.view.x, OUTLINE_COLUMNS + 4);
        press(&mut app, KeyCode::Char('<'));
        press(&mut app, KeyCode::Char('<'));
        draw(&mut app, 120, 20);
        assert_eq!(app.view.x, OUTLINE_COLUMNS - 4);
    }

    #[test]
    fn an_outline_move_to_a_file_with_no_hunks_puts_its_own_name_at_the_top() {
        // Pure renames have no hunks, so their headers sit one under the other.
        let patch: String = (0..30)
            .map(|n| {
                format!(
                    "diff --git a/old{n}.rs b/new{n}.rs\nsimilarity index 100%\nrename from old{n}.rs\nrename to new{n}.rs\n"
                )
            })
            .collect();
        let mut app = App::new(loaded(&patch), Options::default());
        draw(&mut app, 120, 20);
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        for _ in 0..5 {
            press(&mut app, KeyCode::Char('j'));
        }
        let screen = draw(&mut app, 120, 20);
        let top: String = screen
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .skip(OUTLINE_COLUMNS as usize)
            .collect();
        let file = app.current_file().expect("a file is under the cursor");
        let name = app.loaded.diff.files()[file].path().to_owned();
        assert!(top.contains(&name), "{name} is not at the top:\n{screen}");
    }

    #[test]
    fn an_outline_move_brings_that_file_to_the_top_of_the_diff() {
        // Three files, each taller than the screen, so a jump down lands the
        // next file's first line at the bottom unless the view is moved to it.
        let patch: String = ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|name| {
                let added: String = (0..40).map(|n| format!("+{name} line {n}\n")).collect();
                format!(
                    "diff --git a/{name} b/{name}\n--- a/{name}\n+++ b/{name}\n@@ -0,0 +1,40 @@\n{added}"
                )
            })
            .collect();
        let mut app = App::new(loaded(&patch), Options::default());
        draw(&mut app, 120, 20);
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        for (key, name) in [('j', "b.rs"), ('j', "c.rs"), ('k', "b.rs")] {
            press(&mut app, KeyCode::Char(key));
            let screen = draw(&mut app, 120, 20);
            let top: String = screen
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .skip(OUTLINE_COLUMNS as usize)
                .collect();
            assert!(top.contains(name), "{name} is not at the top:\n{screen}");
        }
    }

    #[test]
    fn h_and_l_fold_the_directories_around_the_cursor() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        // The cursor is in `src/core`: h folds that, and the files under it go.
        press(&mut app, KeyCode::Char('h'));
        let side = beside(&draw(&mut app, 120, 20));
        assert!(!side.contains("a.rs"), "{side}");
        assert!(side.contains("main.rs"), "{side}");
        press(&mut app, KeyCode::Char('l'));
        assert!(beside(&draw(&mut app, 120, 20)).contains("a.rs"));
        // h again goes up a directory; l opens them from the inside out.
        press(&mut app, KeyCode::Char('h'));
        press(&mut app, KeyCode::Char('h'));
        let side = beside(&draw(&mut app, 120, 20));
        assert!(!side.contains("main.rs"), "{side}");
        press(&mut app, KeyCode::Char('l'));
        let side = beside(&draw(&mut app, 120, 20));
        assert!(
            !side.contains("a.rs"),
            "the outer fold still holds it:\n{side}"
        );
        press(&mut app, KeyCode::Char('l'));
        let side = beside(&draw(&mut app, 120, 20));
        assert!(side.contains("a.rs"), "{side}");
        assert!(side.contains("main.rs"), "{side}");
    }

    /// `src` holds the files the cursor walks to, and `docs` has one of its
    /// own before them.
    const DEEP: &str = "\
diff --git a/docs/c.md b/docs/c.md
--- a/docs/c.md
+++ b/docs/c.md
@@ -1,1 +1,1 @@
-old c
+new c
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
-old a
+new a
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -1,1 +1,1 @@
-old b
+new b
diff --git a/src/d.rs b/src/d.rs
--- a/src/d.rs
+++ b/src/d.rs
@@ -1,1 +1,1 @@
-old d
+new d
";

    #[test]
    fn folding_from_a_deep_file_leaves_the_other_directories_alone() {
        let mut app = App::new(loaded(DEEP), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        // Past `docs/c.md` and the first two files of `src`, to `src/d.rs`.
        for _ in 0..3 {
            press(&mut app, KeyCode::Char('j'));
            draw(&mut app, 120, 20);
        }
        assert_eq!(app.current_file(), Some(3));
        press(&mut app, KeyCode::Char('h'));
        draw(&mut app, 120, 20);
        press(&mut app, KeyCode::Char('h'));
        let side = beside(&draw(&mut app, 120, 20));
        assert!(
            side.contains("▸ src/"),
            "the deep file's own directory folded:\n{side}"
        );
        assert!(
            side.contains("▾ docs/"),
            "a directory the file does not hang under was folded:\n{side}"
        );
        assert!(side.contains("c.md"), "{side}");
    }
    #[test]
    fn a_file_folded_in_the_diff_opens_when_the_outline_goes_to_it() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('h'));
        assert!(app.folded.contains("src/core/a.rs"));
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.current_file(), Some(1));
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.current_file(), Some(0));
        assert!(app.folded.is_empty(), "going to the file opens it");
    }

    #[test]
    fn a_match_inside_a_folded_directory_is_reached() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.current_file(), Some(2));
        press(&mut app, KeyCode::Char('h'));
        let frame = draw(&mut app, 120, 20);
        assert!(!beside(&frame).contains("links.md"), "{frame}");
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.current_file(), Some(1));
        // A fold holds nothing back from a search.
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "links");
        let frame = draw(&mut app, 120, 20);
        assert!(beside(&frame).contains("links.md"), "{frame}");
        // The match is gone to, and its directory shows around it.
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current_file(), Some(2));
        let frame = draw(&mut app, 120, 20);
        assert!(beside(&frame).contains("links.md"), "{frame}");
    }
    #[test]
    fn t_and_t_toggle_and_switch_the_outline_form() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.form(), Some(OutlineForm::Sidebar));
        press(&mut app, KeyCode::Char('t'));
        assert!(app.outline.is_none());
        press(&mut app, KeyCode::Char('T'));
        assert_eq!(app.form(), Some(OutlineForm::Window));
        // A fold made in one form is kept in the other.
        press(&mut app, KeyCode::Char('h'));
        let shown = |app: &App| -> Vec<String> {
            let open = app.outline.as_ref().expect("the outline is open");
            open.tree
                .visible()
                .iter()
                .map(|&node| open.tree.nodes[node].label.clone())
                .collect()
        };
        let folded = shown(&app);
        assert!(!folded.iter().any(|label| label == "a.rs"), "{folded:?}");
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.form(), Some(OutlineForm::Sidebar));
        assert_eq!(shown(&app), folded);
        press(&mut app, KeyCode::Char('T'));
        assert_eq!(app.form(), Some(OutlineForm::Window));
        assert_eq!(shown(&app), folded);
        press(&mut app, KeyCode::Char('T'));
        assert!(app.outline.is_none());
    }
    #[test]
    fn a_window_over_the_diff_takes_every_key() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('T'));
        let before = (
            app.current_file(),
            app.toast.as_ref().map(|toast| toast.text.clone()),
        );
        // Nothing reaches the diff behind the window: not its overview, its
        // reloads, nor its next file.
        for key in ['O', 'r', 'o', ']'] {
            press(&mut app, KeyCode::Char(key));
        }
        assert!(app.overview.is_none());
        assert_eq!(app.current_file(), before.0);
        assert_eq!(app.toast.as_ref().map(|toast| toast.text.clone()), before.1);
        assert_eq!(app.form(), Some(OutlineForm::Window));
        // `q` closes the window and leaves the diff open.
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.form(), None);
        assert!(!app.quit);
    }
    #[test]
    fn a_diff_without_files_says_there_is_nothing_to_outline() {
        let mut app = App::new(loaded(""), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        press(&mut app, KeyCode::Char('t'));
        assert!(app.outline.is_none());
        assert_eq!(
            app.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("This diff has no files")
        );
    }

    #[test]
    fn a_click_on_the_outline_is_not_a_click_on_the_diff() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        draw(&mut app, 120, 20);
        let before = (app.cursor, app.scroll);
        click(&mut app, 3, 6);
        assert_eq!((app.cursor, app.scroll), before);
        // The diff itself still takes a click.
        click(&mut app, 100, 6);
        assert_ne!((app.cursor, app.scroll), before);
    }

    /// Files of `lines` rows each, so the diff is taller than the screen.
    fn tall_tree(lines: usize) -> String {
        let mut patch = String::new();
        for name in ["a", "b"] {
            patch.push_str(&format!(
                "diff --git a/{name}.rs b/{name}.rs\n--- a/{name}.rs\n+++ b/{name}.rs\n@@ -1,{lines} +1,{lines} @@\n"
            ));
            for line in 1..=lines {
                patch.push_str(&format!(" line {line}\n"));
            }
        }
        patch
    }

    #[test]
    fn the_outline_follows_the_diff_as_the_wheel_moves_it() {
        let mut app = App::new(loaded(&tall_tree(30)), Options::default());
        app.view = Rect::new(0, 1, 120, 20);
        press(&mut app, KeyCode::Char('t'));
        assert!(picked(&draw(&mut app, 120, 20)).contains("a.rs"));
        // The cursor is the authority: a wheel over the panel scrolls the
        // diff, and the panel marks the file the cursor came to.
        for _ in 0..10 {
            wheel(&mut app, MouseEventKind::ScrollDown);
        }
        let screen = draw(&mut app, 120, 20);
        assert_eq!(app.current_file(), Some(1));
        assert!(picked(&screen).contains("b.rs"), "{screen}");
    }

    #[test]
    fn a_reload_changes_the_tree() {
        let mut app = tree_app();
        press(&mut app, KeyCode::Char('t'));
        assert!(beside(&draw(&mut app, 120, 20)).contains("main.rs"));
        let remember = app.remember();
        app.finish(Done::Load {
            result: Box::new(Ok(loaded(TWO_FILES))),
            remember,
            background: true,
        });
        let side = beside(&draw(&mut app, 120, 20));
        assert!(side.contains("a.rs") && side.contains("b.rs"), "{side}");
        assert!(!side.contains("main.rs"), "{side}");
    }

    #[test]
    fn a_screen_too_narrow_for_both_keeps_the_diff_alone() {
        let mut app = tree_app();
        app.view.width = OUTLINE_MIN_SCREEN - 1;
        press(&mut app, KeyCode::Char('t'));
        assert!(app.outline.is_none());
        assert_eq!(
            app.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("The screen is too narrow for the outline")
        );
        app.view.width = OUTLINE_MIN_SCREEN;
        press(&mut app, KeyCode::Char('t'));
        assert!(app.outline.is_some());
        draw(&mut app, OUTLINE_MIN_SCREEN - 1, 20);
        assert!(app.outline.is_none());
    }
}
