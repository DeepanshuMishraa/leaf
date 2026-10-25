//! The diff reviewer: a virtualized patch view, line selection, comments, and
//! sending them to an agent or a pull request.

use std::cell::RefCell;
use std::collections::HashMap;
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
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::agent;
use crate::comment::{Anchor, Comment, Side, State};
use crate::diff::{self, Diff, Kind as RowKind, Loaded, Request};
use crate::github;
use crate::progress::{SavedComment, Store, StoreFailed};
use crate::render::DIM;
use crate::syntax;

const ACCENT: Color = Color::Indexed(12);
const ADDED_FG: Color = Color::Indexed(2);
const REMOVED_FG: Color = Color::Indexed(1);
const NOTE: Color = Color::Indexed(3);
/// How many hunks may be lexed in one frame; the rest stay plain until the
/// reader pauses. Keeps a fast scroll from spending the frame in the lexer.
const HIGHLIGHT_BUDGET: u8 = 1;
/// Hunks kept lexed before the whole cache is dropped.
const HIGHLIGHT_CACHE: usize = 64;
/// A hunk longer than this stays plain; lexing it would cost more than it adds.
const HIGHLIGHT_BYTES: usize = 128 * 1024;
/// How many rows one wheel notch moves.
const WHEEL_ROWS: usize = 3;
/// Width below which only one line-number column fits.
const FULL_GUTTER_MIN: u16 = 60;

/// Warm backgrounds chosen for the terminal's paper, so text keeps its contrast.
struct Palette {
    added: Color,
    removed: Color,
    added_word: Color,
    removed_word: Color,
    file: Color,
}

impl Palette {
    #[cfg(not(test))]
    fn detect() -> Self {
        use std::io::IsTerminal;

        if !io::stdin().is_terminal() {
            return Palette::dark();
        }
        let light = crate::viewer::query_color(11)
            .map(|[r, g, b]| {
                (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000 > 140
            })
            .unwrap_or(false);
        if light {
            Palette {
                added: Color::Indexed(194),
                removed: Color::Indexed(224),
                added_word: Color::Indexed(157),
                removed_word: Color::Indexed(217),
                file: Color::Indexed(254),
            }
        } else {
            Palette {
                added: Color::Indexed(22),
                removed: Color::Indexed(52),
                added_word: Color::Indexed(28),
                removed_word: Color::Indexed(88),
                file: Color::Indexed(236),
            }
        }
    }

    #[cfg(test)]
    fn detect() -> Self {
        Palette::dark()
    }

    fn dark() -> Self {
        Palette {
            added: Color::Indexed(22),
            removed: Color::Indexed(52),
            added_word: Color::Indexed(28),
            removed_word: Color::Indexed(88),
            file: Color::Indexed(236),
        }
    }
}

/// Comments and the store they persist in.
pub struct Options {
    pub comments: Vec<SavedComment>,
    pub store: Option<Store>,
    /// Store failures are reported after the terminal is restored.
    pub failure: Rc<RefCell<Option<String>>>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            comments: Vec::new(),
            store: None,
            failure: Rc::new(RefCell::new(None)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogKind {
    Help,
    Comments,
    Error,
    Confirm,
    Loading,
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
    Comments { selected: usize },
    Error(String),
    Confirm(Confirm),
    Loading(String),
}

impl Dialog {
    fn kind(&self) -> DialogKind {
        match self {
            Dialog::None => unreachable!(),
            Dialog::Help => DialogKind::Help,
            Dialog::Comments { .. } => DialogKind::Comments,
            Dialog::Error(_) => DialogKind::Error,
            Dialog::Confirm(_) => DialogKind::Confirm,
            Dialog::Loading(_) => DialogKind::Loading,
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

/// The comment being written, and the lines it hangs off.
#[derive(Clone)]
struct Composer {
    text: Vec<char>,
    cursor: usize,
    anchor: Anchor,
    first: usize,
    last: usize,
}

enum Job {
    Load {
        rx: mpsc::Receiver<Result<Loaded, diff::DiffFailed>>,
        remember: Option<(String, Side, u32)>,
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

struct App {
    loaded: Loaded,
    palette: Palette,
    cursor: usize,
    scroll: usize,
    left: u16,
    selection: Option<usize>,
    drag_origin: Option<usize>,
    composer: Option<Composer>,
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
    /// The diff's rows on screen.
    view: Rect,
    /// Rows inside the comment box, for moving the cursor.
    composer_view: u16,
    highlight: HashMap<(u32, u32), Rc<Vec<Vec<syntax::Kind>>>>,
    new_highlights: u8,
    deliver: Deliver,
}

impl App {
    fn new(loaded: Loaded, options: Options) -> Self {
        let cursor = loaded.diff.first_row().unwrap_or(0);
        let key = loaded.key.clone();
        let mut app = App {
            loaded,
            palette: Palette::detect(),
            cursor,
            scroll: 0,
            left: 0,
            selection: None,
            drag_origin: None,
            composer: None,
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
            view: Rect::default(),
            composer_view: 0,
            highlight: HashMap::new(),
            new_highlights: HIGHLIGHT_BUDGET,
            deliver: Box::new(agent::deliver),
        };
        app.rebuild_marks();
        app
    }

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
        loop {
            self.receive_job();
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
                match event::read()? {
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

    fn receive_job(&mut self) {
        let Some(job) = self.job.take() else {
            return;
        };
        let done = match job {
            Job::Load { rx, remember } => match rx.try_recv() {
                Ok(result) => Some(Done::Load {
                    result: Box::new(result),
                    remember,
                }),
                Err(TryRecvError::Empty) => {
                    self.job = Some(Job::Load { rx, remember });
                    None
                }
                Err(TryRecvError::Disconnected) => Some(Done::Load {
                    result: Box::new(Err(diff::DiffFailed::Git {
                        command: "leaf".to_owned(),
                        cause: "the loader stopped before returning a diff".to_owned(),
                    })),
                    remember,
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
            Done::Load { result, remember } => match *result {
                Ok(loaded) => {
                    let same_key = loaded.key == self.key;
                    self.loaded = loaded;
                    self.key = self.loaded.key.clone();
                    if !same_key {
                        self.load_comments();
                    }
                    self.cursor = remember
                        .and_then(|(file, side, line)| self.loaded.diff.row_for(&file, side, line))
                        .or_else(|| self.loaded.diff.first_row())
                        .unwrap_or(0);
                    self.scroll = self.cursor.saturating_sub(self.view.height as usize / 2);
                    self.selection = None;
                    self.composer = None;
                    self.left = 0;
                    self.highlight.clear();
                    self.dialog = Dialog::None;
                    let _ = execute!(io::stdout(), SetTitle(&self.loaded.title));
                    self.toast(format!("Loaded {}", self.loaded.label));
                }
                Err(e) => {
                    self.dialog = Dialog::Error(e.to_string());
                }
            },
            Done::Post {
                result,
                index,
                composer,
                added,
            } => match result {
                Ok(pull) => {
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

/// Opens the reviewer on a loaded patch.
pub fn run(loaded: Loaded, options: Options) -> io::Result<()> {
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
    let mut app = App::new(loaded, options);
    let result = setup.and_then(|_| app.event_loop(&mut terminal));
    let _ = execute!(io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// The text pasted into an agent or posted as a comment: a quoted line range,
/// the patch lines, then the comment.
fn payload(diff: &Diff, anchor: &Anchor, first: usize, last: usize, body: &str) -> String {
    let mut out = format!(
        "> {} {}:\n\n```diff\n{}\n```",
        anchor.file,
        anchor.range_label(),
        diff.snippet(first, last)
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
    Send,
    Draft,
    Github,
}

impl Composer {
    fn new(anchor: Anchor, first: usize, last: usize) -> Self {
        Composer {
            text: Vec::new(),
            cursor: 0,
            anchor,
            first,
            last,
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
            KeyCode::Enter => Edit::Send,
            KeyCode::Esc => Edit::Draft,
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
        if self.dialog_kind().is_some() {
            self.dialog_key(key);
            return;
        }
        if self.composer.is_some() {
            self.composer_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(true),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(false),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page(true, true)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.page(false, true)
            }
            KeyCode::PageDown | KeyCode::Char(' ') => self.page(true, false),
            KeyCode::PageUp | KeyCode::Char('b') => self.page(false, false),
            KeyCode::Char('g') | KeyCode::Home => self.go_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.go_edge(true),
            KeyCode::Char(']') | KeyCode::Char('n') => self.next_file(true),
            KeyCode::Char('[') | KeyCode::Char('N') => self.next_file(false),
            KeyCode::Char('v') => self.toggle_selection(),
            KeyCode::Char('c') => self.compose(),
            KeyCode::Char('C') => self.show_comments(),
            KeyCode::Char('y') => self.copy_payload(false),
            KeyCode::Char('Y') => self.copy_payload(true),
            KeyCode::Char('o') => self.other_side(),
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('h') | KeyCode::Left => {
                self.left = self.left.saturating_sub(4);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.left = self.left.saturating_add(4);
            }
            KeyCode::Char('0') => self.left = 0,
            KeyCode::Char('?') => self.dialog = Dialog::Help,
            _ => {}
        }
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let action = match &mut self.composer {
            Some(composer) => composer.edit(key),
            None => Edit::None,
        };
        match action {
            Edit::None => {}
            Edit::Send => self.send_to_agent(),
            Edit::Draft => self.save_draft(),
            Edit::Github => self.ask_github(),
        }
    }

    fn dialog_key(&mut self, key: KeyEvent) {
        match self.dialog_kind() {
            Some(DialogKind::Comments) => self.comments_key(key),
            Some(DialogKind::Confirm) => self.confirm_key(key),
            Some(DialogKind::Loading) => {}
            Some(DialogKind::Help | DialogKind::Error) | None => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
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
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('C') => self.dialog = Dialog::None,
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
        if self.dialog_kind().is_some() || self.composer.is_some() {
            return;
        }
        let (top, height, scroll) = (self.view.y, self.view.height, self.scroll);
        let row_at = |y: u16| -> Option<usize> {
            let inside = y.checked_sub(top)? as usize;
            (inside < height as usize).then_some(scroll + inside)
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.scroll = (self.scroll + WHEEL_ROWS).min(self.max_scroll());
                self.cursor = self.snap(self.cursor.max(self.scroll));
            }
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(WHEEL_ROWS);
                self.cursor = self.snap(self.cursor.min(self.scroll + self.view.height as usize));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(row) = row_at(mouse.row)
                    && row < self.loaded.diff.rows().len()
                {
                    let row = self.snap(row);
                    self.cursor = row;
                    self.selection = None;
                    self.drag_origin = Some(row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(origin) = self.drag_origin
                    && let Some(row) = row_at(mouse.row)
                    && row < self.loaded.diff.rows().len()
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
        if let Some(composer) = &mut self.composer {
            composer.insert(text);
        }
    }

    fn max_scroll(&self) -> usize {
        self.loaded
            .diff
            .rows()
            .len()
            .saturating_sub(self.view.height as usize)
    }

    fn snap(&self, row: usize) -> usize {
        let diff = &self.loaded.diff;
        if diff.rows().get(row).is_some_and(|r| r.kind.is_content()) {
            return row;
        }
        diff.next_content(row, true)
            .or_else(|| diff.next_content(row, false))
            .unwrap_or(row.min(diff.rows().len().saturating_sub(1)))
    }

    fn move_cursor(&mut self, forward: bool) {
        if let Some(next) = self.loaded.diff.next_content(self.cursor, forward) {
            self.cursor = next;
        }
        self.follow_cursor();
    }

    fn page(&mut self, forward: bool, half: bool) {
        let step = (self.view.height as usize).max(1);
        let step = if half {
            (step / 2).max(1)
        } else {
            step.saturating_sub(1).max(1)
        };
        let last = self.loaded.diff.rows().len().saturating_sub(1);
        let target = if forward {
            (self.cursor + step).min(last)
        } else {
            self.cursor.saturating_sub(step)
        };
        self.cursor = self.snap(target);
        self.follow_cursor();
    }

    fn go_edge(&mut self, end: bool) {
        let diff = &self.loaded.diff;
        self.cursor = if end {
            diff.last_row()
        } else {
            diff.first_row()
        }
        .unwrap_or(0);
        self.follow_cursor();
    }

    fn next_file(&mut self, forward: bool) {
        let diff = &self.loaded.diff;
        if diff.files().is_empty() {
            return;
        }
        let current = diff.file_index(self.cursor).unwrap_or(0);
        let index = if forward {
            (current + 1) % diff.files().len()
        } else {
            (current + diff.files().len() - 1) % diff.files().len()
        };
        let count = diff.files().len();
        if let Some(row) = diff.first_content(index) {
            self.cursor = row;
            self.selection = Some(row);
            self.follow_cursor();
            self.toast(format!("{} of {count}", index + 1));
        }
    }

    fn follow_cursor(&mut self) {
        let height = self.view.height as usize;
        if height == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn toggle_selection(&mut self) {
        if self.selection.is_some() {
            self.selection = None;
            self.toast("Selection cleared");
        } else {
            self.selection = Some(self.cursor);
            self.toast("Selecting: move to extend, c to comment, Esc to clear");
        }
    }

    fn range(&self) -> (usize, usize) {
        let anchor = self.selection.unwrap_or(self.cursor);
        (anchor.min(self.cursor), anchor.max(self.cursor))
    }

    fn compose(&mut self) {
        let (start, end) = self.range();
        match self.loaded.diff.comment_range(start, end) {
            Some((first, last, anchor)) => {
                self.cursor = last;
                self.selection = Some(first);
                self.composer = Some(Composer::new(anchor, first, last));
                self.follow_cursor();
            }
            None => self.fail(
                "That range has no code line to comment on. Move to a line that changed, then press c.",
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
                self.selection = Some(composer.first);
                self.toast(format!("Pasted into {}", agent.describe()));
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

    fn save_draft(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let body = composer.body();
        if body.is_empty() {
            self.toast("Empty comment discarded");
            return;
        }
        self.add_comment(Comment {
            anchor: composer.anchor,
            body,
            state: State::Draft,
        });
        self.toast("Draft saved: C lists comments");
    }

    fn ask_github(&mut self) {
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
                self.toast(format!("Pasted into {}", agent.describe()));
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
            self.toast("No comments yet: select lines with v, then press c");
            return;
        }
        self.dialog = Dialog::Comments { selected: 0 };
    }

    fn jump_to(&mut self, anchor: &Anchor) {
        if let Some(row) = self
            .loaded
            .diff
            .row_for(&anchor.file, anchor.side, anchor.start_line)
        {
            self.cursor = row;
            self.selection = Some(row);
            self.follow_cursor();
            self.toast(anchor.label());
        } else {
            self.fail(format!(
                "{} is not in this diff. Reload with r, then try again.",
                anchor.label()
            ));
        }
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
        if let Some((first, last, anchor)) = self.loaded.diff.comment_range(start, end) {
            self.copy = Some(payload(&self.loaded.diff, &anchor, first, last, ""));
            self.toast(format!("Copied {} {}", anchor.file, anchor.range_label()));
        } else {
            self.fail("That range has no code line to copy. Move to a changed line, then press y.");
        }
    }

    fn remember(&self) -> Option<(String, Side, u32)> {
        let diff = &self.loaded.diff;
        let row = diff.rows().get(self.cursor)?;
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

    fn start_load(&mut self, request: Request, message: &str) {
        let remember = self.remember();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(request.load());
        });
        self.job = Some(Job::Load { rx, remember });
        self.dialog = Dialog::Loading(message.to_owned());
    }

    fn reload(&mut self) {
        self.start_load(self.loaded.request.clone(), "Reloading the diff…");
    }

    fn other_side(&mut self) {
        match self.loaded.request.other_side() {
            Some(request) => {
                let message = request.label();
                self.start_load(request, &format!("Loading {message}…"));
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
        let header = Rect::new(area.x, area.y, area.width, 1);
        let footer = if area.height >= 5 { 2 } else { 1 };
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1 + footer),
        );
        self.view = body;
        self.follow_cursor();
        self.draw_header(f, header);
        self.draw_rows(f, body);
        self.draw_footer(
            f,
            Rect::new(
                area.x,
                area.y + area.height.saturating_sub(footer),
                area.width,
                footer,
            ),
        );
        self.draw_composer(f, body);
        if let Some(kind) = self.dialog_kind() {
            self.draw_dialog(f, area, kind);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let diff = &self.loaded.diff;
        let (added, removed) = totals(diff);
        let mut spans = vec![
            Span::styled(
                " leaf diff ",
                Style::default()
                    .fg(Color::Indexed(0))
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", self.loaded.label),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(branch) = &self.loaded.branch {
            spans.push(Span::styled(format!("{branch} "), Style::default().fg(DIM)));
        }
        let stats = format!(
            " +{} −{} · {} {} ",
            added,
            removed,
            diff.files().len(),
            if diff.files().len() == 1 {
                "file"
            } else {
                "files"
            }
        );
        let used: usize = spans.iter().map(|span| cells(&span.content)).sum();
        let free = (area.width as usize).saturating_sub(used + cells(&stats));
        if free > 0 {
            spans.push(Span::raw(" ".repeat(free)));
        }
        spans.push(Span::styled(stats, Style::default().fg(DIM)));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let status = self.status_line();
        let first = if let Some(toast) = &self.toast {
            let style = if toast.error {
                Style::default()
                    .fg(Color::Indexed(1))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(ACCENT)
            };
            let text = clip(&toast.text, area.width.saturating_sub(2) as usize);
            Line::from(Span::styled(format!(" {text}"), style))
        } else {
            Line::from(status)
        };
        let keys = clip(
            " j/k move · v select · c comment · C comments · y copy · o other side · r reload · ? help · q quit",
            area.width as usize,
        );
        let mut lines = vec![first];
        if area.height > 1 {
            lines.push(Line::from(Span::styled(keys, Style::default().fg(DIM))));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn status_line(&self) -> Vec<Span<'static>> {
        let diff = &self.loaded.diff;
        let Some(row) = diff.rows().get(self.cursor) else {
            return vec![Span::raw(" ")];
        };
        let mut spans = Vec::new();
        if let Some(file) = diff.file(row.file as usize) {
            spans.push(Span::styled(
                format!(" {} ", file.path()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        if row.kind.is_content() {
            let (line, side) = match row.kind {
                RowKind::Removed => (row.old, "old"),
                _ => (row.new, "new"),
            };
            spans.push(Span::styled(
                format!("L{line} {side} "),
                Style::default().fg(DIM),
            ));
        }
        if self.selection.is_some() {
            let (first, last) = self.range();
            let rows = diff.rows();
            let lines = rows[first..=last.min(rows.len().saturating_sub(1))]
                .iter()
                .filter(|row| row.kind.is_content())
                .count();
            spans.push(Span::styled(
                format!("· {lines} selected · c comment "),
                Style::default().fg(NOTE),
            ));
        }
        spans
    }

    fn draw_rows(&mut self, f: &mut Frame, area: Rect) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let diff = &self.loaded.diff;
        if diff.rows().is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " This diff is empty: nothing changed on this side.",
                    Style::default().fg(DIM),
                ))),
                area,
            );
            return;
        }
        let end = (self.scroll + area.height as usize).min(diff.rows().len());
        let mut lines = Vec::with_capacity(end - self.scroll);
        for index in self.scroll..end {
            lines.push(self.row_line(index, area.width));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn row_line(&mut self, index: usize, width: u16) -> Line<'static> {
        let diff = &self.loaded.diff;
        let row = diff.rows()[index];
        match row.kind {
            RowKind::File => {
                let file = diff.file(row.file as usize);
                let (added, removed) = file.map_or((0, 0), |file| (file.added, file.removed));
                let path = file.map(|file| file.display()).unwrap_or_default();
                let note = file.and_then(|file| file.note.clone());
                let mut spans = vec![
                    Span::styled(
                        "▾ ",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        path,
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                ];
                if added > 0 {
                    spans.push(Span::styled(
                        format!(" +{added}"),
                        Style::default().fg(ADDED_FG),
                    ));
                }
                if removed > 0 {
                    spans.push(Span::styled(
                        format!(" −{removed}"),
                        Style::default().fg(REMOVED_FG),
                    ));
                }
                if let Some(note) = note {
                    spans.push(Span::styled(format!("  {note}"), Style::default().fg(NOTE)));
                }
                Line::from(spans)
            }
            RowKind::Hunk => Line::from(Span::styled(
                format!("  {}", diff.row_text(index)),
                Style::default().fg(DIM),
            )),
            RowKind::Meta | RowKind::NoNewline => Line::from(Span::styled(
                format!("  {}", diff.row_text(index)),
                Style::default().fg(NOTE).add_modifier(Modifier::ITALIC),
            )),
            RowKind::Context | RowKind::Added | RowKind::Removed => self.content_line(index, width),
        }
    }

    fn content_line(&mut self, index: usize, width: u16) -> Line<'static> {
        let diff = &self.loaded.diff;
        let row = diff.rows()[index];
        let number = self.number_width();
        let two = width >= FULL_GUTTER_MIN;
        let mark = self.marks.get(&index).copied();
        let gutter = if two { number * 2 + 4 } else { number + 3 };
        let selected = self.selection.is_some_and(|anchor| {
            index >= anchor.min(self.cursor) && index <= anchor.max(self.cursor)
        });
        let tint = match row.kind {
            RowKind::Added => Some(self.palette.added),
            RowKind::Removed => Some(self.palette.removed),
            _ => None,
        };
        let word = match row.kind {
            RowKind::Added => Some(self.palette.added_word),
            RowKind::Removed => Some(self.palette.removed_word),
            _ => None,
        };
        let base = match tint {
            Some(tint) => Style::default().bg(tint),
            None => Style::default(),
        };
        let gutter_style = if index == self.cursor {
            Style::default()
                .fg(ACCENT)
                .bg(tint.unwrap_or(Color::Reset))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM).bg(tint.unwrap_or(Color::Reset))
        };
        let mut spans = Vec::new();
        if let Some(state) = mark {
            let color = match state {
                State::Draft => DIM,
                State::Agent => ACCENT,
                State::Github => ADDED_FG,
            };
            spans.push(Span::styled(
                "●",
                Style::default().fg(color).bg(tint.unwrap_or(Color::Reset)),
            ));
        } else {
            spans.push(Span::styled(
                " ",
                Style::default().bg(tint.unwrap_or(Color::Reset)),
            ));
        }
        let label = if two {
            format!(
                "{:>number$} {:>number$} ",
                if row.old > 0 {
                    row.old.to_string()
                } else {
                    String::new()
                },
                if row.new > 0 {
                    row.new.to_string()
                } else {
                    String::new()
                },
            )
        } else {
            format!(
                "{:>number$} ",
                if row.new > 0 {
                    row.new.to_string()
                } else if row.old > 0 {
                    row.old.to_string()
                } else {
                    String::new()
                }
            )
        };
        spans.push(Span::styled(label, gutter_style));

        let text = diff.content(index).to_owned();
        let marker = diff.row_text(index).chars().next().unwrap_or(' ');
        let marker_style = match row.kind {
            RowKind::Added => base.fg(ADDED_FG).add_modifier(Modifier::BOLD),
            RowKind::Removed => base.fg(REMOVED_FG).add_modifier(Modifier::BOLD),
            _ => base.fg(DIM),
        };
        spans.push(Span::styled(marker.to_string(), marker_style));

        let kinds = self.kinds_for(index);
        let line_index = self.hunk_line(index);
        let kinds_line = kinds
            .as_ref()
            .and_then(|lines| lines.get(line_index))
            .map(Vec::as_slice);
        let emphasis = self.emphasis(index);
        let available = (width as usize).saturating_sub(gutter);
        let selection_bg = Some(self.palette.file);
        let used = text_spans(
            &mut spans,
            &text,
            self.left as usize,
            available,
            base,
            kinds_line,
            emphasis,
            word.unwrap_or(Color::Reset),
            if selected { selection_bg } else { None },
        );
        let filled = (gutter + 1 + used).min(width as usize);
        if let Some(tint) = tint {
            spans.push(Span::styled(
                " ".repeat((width as usize).saturating_sub(filled)),
                Style::default().bg(tint),
            ));
        }
        Line::from(spans)
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

    /// The unchanged prefix and suffix of a changed line, in characters,
    /// against the line it replaced. The rest is the word-level change.
    fn emphasis(&self, row_index: usize) -> Option<(usize, usize)> {
        let diff = &self.loaded.diff;
        let row = *diff.rows().get(row_index)?;
        let hunk = diff.file(row.file as usize)?.hunks.get(row.hunk as usize)?;
        let rows = diff.rows();
        let pair = match row.kind {
            RowKind::Removed => {
                let mut start = row_index;
                while start > hunk.first && rows[start - 1].kind == RowKind::Removed {
                    start -= 1;
                }
                let mut added = row_index;
                while added < hunk.end && rows[added].kind == RowKind::Removed {
                    added += 1;
                }
                let pair = added + (row_index - start);
                (rows.get(pair)?.kind == RowKind::Added).then_some(pair)?
            }
            RowKind::Added => {
                let mut start = row_index;
                while start > hunk.first && rows[start - 1].kind == RowKind::Added {
                    start -= 1;
                }
                let mut removed = start;
                while removed > hunk.first && rows[removed - 1].kind == RowKind::Removed {
                    removed -= 1;
                }
                let pair = removed + (row_index - start);
                (rows.get(pair)?.kind == RowKind::Removed).then_some(pair)?
            }
            _ => return None,
        };
        let (a, b) = (diff.content(pair), diff.content(row_index));
        let prefix = a.chars().zip(b.chars()).take_while(|(a, b)| a == b).count();
        let suffix = a
            .chars()
            .rev()
            .zip(b.chars().rev())
            .take_while(|(a, b)| a == b)
            .count()
            .min(a.chars().count().saturating_sub(prefix))
            .min(b.chars().count().saturating_sub(prefix));
        Some((prefix, suffix))
    }

    fn draw_composer(&mut self, f: &mut Frame, area: Rect) {
        let Some(composer) = self.composer.as_ref() else {
            return;
        };
        let width = area.width.clamp(20, 88);
        let height = area.height.min(9);
        if height < 4 {
            return;
        }
        let rect = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height - height,
            width,
            height,
        );
        f.render_widget(Clear, rect);
        let title = format!(
            " comment · {} {} ",
            composer.anchor.file,
            composer.anchor.range_label()
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(Line::from(Span::styled(title, Style::default().fg(ACCENT))));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let (lines, cursor_line) = composer.wrap(inner.width as usize);
        let rows = inner.height as usize;
        let mut view = self.composer_view as usize;
        if cursor_line < view {
            view = cursor_line;
        } else if cursor_line >= view + rows {
            view = cursor_line + 1 - rows;
        }
        view = view.min(lines.len().saturating_sub(rows));
        self.composer_view = view as u16;

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
        f.render_widget(Paragraph::new(rendered), inner);
    }

    fn draw_dialog(&mut self, f: &mut Frame, area: Rect, kind: DialogKind) {
        match kind {
            DialogKind::Help => draw_help(f, area),
            DialogKind::Loading => {
                let text = match &self.dialog {
                    Dialog::Loading(text) => text.clone(),
                    _ => String::new(),
                };
                panel(
                    f,
                    area,
                    "loading",
                    vec![
                        Line::from(Span::styled(text, Style::default().fg(ACCENT))),
                        Line::from(Span::styled(
                            "This takes a moment.",
                            Style::default().fg(DIM),
                        )),
                    ],
                    48,
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
                panel(f, area, "could not finish", lines, width);
            }
            DialogKind::Comments => self.draw_comments(f, area),
            DialogKind::Confirm => self.draw_confirm(f, area),
        }
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
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), style),
                Span::styled(saved.comment.label(), style),
                Span::styled(
                    format!("  {}", saved.comment.state.label()),
                    Style::default().fg(DIM),
                ),
            ]));
            let body = clip(
                &saved.comment.body.replace('\n', " "),
                width.saturating_sub(8) as usize,
            );
            lines.push(Line::from(Span::styled(
                format!("   {body}"),
                Style::default().fg(DIM),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Enter jump · a agent · g GitHub · d delete · y copy · Esc close",
            Style::default().fg(DIM),
        )));
        if lines.len() + 4 > area.height as usize {
            lines.truncate(area.height.saturating_sub(5) as usize);
        }
        panel(f, area, "comments", lines, width);
    }

    fn draw_confirm(&self, f: &mut Frame, area: Rect) {
        let Dialog::Confirm(confirm) = &self.dialog else {
            return;
        };
        let width = 74.min(area.width.saturating_sub(8)).max(24);
        let target = self
            .loaded
            .github
            .as_ref()
            .map(|resource| resource.label())
            .unwrap_or_else(|| "the pull request for this branch".to_owned());
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Post this comment to {target}?"),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                confirm.anchor.label(),
                Style::default().fg(ACCENT),
            )),
        ];
        for line in wrap_text(&confirm.body, width.saturating_sub(8) as usize)
            .into_iter()
            .take(8)
        {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(DIM),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "y",
                Style::default().fg(ADDED_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" post    ", Style::default().fg(DIM)),
            Span::styled(
                "n",
                Style::default().fg(REMOVED_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" back to editing", Style::default().fg(DIM)),
        ]));
        panel(f, area, "post to github", lines, width);
    }

    fn has_pending_highlight(&self) -> bool {
        let diff = &self.loaded.diff;
        let end = (self.scroll + self.view.height as usize).min(diff.rows().len());
        (self.scroll..end).any(|index| {
            let row = diff.rows()[index];
            row.kind.is_content()
                && row.hunk != u32::MAX
                && !self.highlight.contains_key(&(row.file, row.hunk))
        })
    }
}

fn totals(diff: &Diff) -> (u32, u32) {
    diff.files().iter().fold((0, 0), |(added, removed), file| {
        (added + file.added, removed + file.removed)
    })
}

fn cells(text: &str) -> usize {
    text.chars()
        .map(|ch| {
            if ch == '\t' {
                4
            } else {
                ch.width().unwrap_or(0)
            }
        })
        .sum()
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
    emphasis: Option<(usize, usize)>,
    word: Color,
    selection: Option<Color>,
) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let (prefix, suffix) = emphasis.unwrap_or((0, 0));
    let changed_from = prefix;
    let changed_to = chars.len().saturating_sub(suffix);
    let mut column = 0;
    let mut used = 0;
    for (at, ch) in chars.iter().enumerate() {
        let mut style = base;
        if let Some(kinds) = kinds
            && let Some(kind) = kinds.get(at)
        {
            style = style.patch(crate::render::code_style(*kind));
        }
        if at >= changed_from && at < changed_to {
            style = style.bg(word);
        }
        if let Some(selection) = selection {
            style = style.bg(selection);
        }
        if *ch == '\t' {
            let width = 4 - column % 4;
            if column >= start {
                if used + width > avail {
                    break;
                }
                push_text(spans, style, &" ".repeat(width));
                used += width;
            }
            column += width;
            continue;
        }
        let width = ch.width().unwrap_or(0);
        if column + width <= start {
            column += width;
            continue;
        }
        if used + width > avail {
            break;
        }
        if column >= start {
            let mut buffer = [0u8; 4];
            push_text(spans, style, ch.encode_utf8(&mut buffer));
            used += width;
        }
        column += width;
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

fn draw_help(f: &mut Frame, area: Rect) {
    let keys = [
        ("j k  ↑ ↓", "move between code lines"),
        ("d u", "half page down or up"),
        ("space b", "page down or up"),
        ("g G  Home End", "first or last line"),
        ("[ ]  N n", "previous or next file"),
        ("v", "select lines (move to extend)"),
        ("c", "comment on the selection"),
        ("C", "comments: jump, resend, delete, post"),
        ("y Y", "copy this range or every comment"),
        ("o r", "other side of the diff, or reload"),
        ("h l  ← →", "scroll sideways; 0 resets"),
        ("? q", "this help, quit"),
    ];
    let mut lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<16}"), Style::default().fg(ACCENT)),
                Span::styled(*what, Style::default()),
            ])
        })
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " c, write, Enter → pastes into the agent pane.",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " Alt+Enter adds a line · Ctrl+G posts to GitHub · Esc saves a draft.",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        " y copies a prompt for any agent. LEAF_AGENT_PANE=%3 names the pane.",
        Style::default().fg(DIM),
    )));
    panel(f, area, "keys", lines, 78);
}

fn panel(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line<'static>>, width: u16) -> Rect {
    let width = width.saturating_add(8).min(area.width);
    let height = (lines.len() as u16).saturating_add(4).min(area.height);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(
            Line::from(format!(" {title} "))
                .style(Style::default().fg(DIM))
                .centered(),
        );
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

impl App {
    fn wait_duration(&self) -> Option<Duration> {
        if self.job.is_some() {
            return Some(Duration::from_millis(40));
        }
        if let Some(toast) = &self.toast {
            return Some(toast.until.saturating_duration_since(Instant::now()));
        }
        if self.has_pending_highlight() {
            return Some(Duration::from_millis(16));
        }
        None
    }
}

// PART4

#[cfg(test)]
mod tests {
    use super::*;
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
    fn a_selection_comments_the_lines_it_covers() {
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
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('c'));

        let composer = app.composer.as_ref().expect("a comment box");
        assert_eq!(composer.anchor.file, "src/main.rs");
        assert_eq!(composer.anchor.side, Side::New);
        assert_eq!(
            (composer.anchor.start_line, composer.anchor.end_line),
            (11, 11)
        );
        assert_eq!((composer.first, composer.last), (3, 4));

        type_text(&mut app, "why?");
        press(&mut app, KeyCode::Enter);
        assert!(app.composer.is_none());
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.body, "why?");
        assert_eq!(app.comments[0].comment.state, State::Agent);

        let text = sent.borrow().clone().expect("the payload");
        assert!(text.contains("> src/main.rs L11:"));
        assert!(text.contains("```diff\n-let b = 2;\n+let b = 3;\n```"));
        assert!(text.ends_with("why?\n"));
    }

    #[test]
    fn failed_agent_delivery_keeps_a_draft_and_copies_the_prompt() {
        let mut app = app();
        app.deliver = Box::new(|_| Err(agent::AgentFailed::NoTmux));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "check this");
        press(&mut app, KeyCode::Enter);

        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.state, State::Draft);
        assert!(app.comments.is_empty() || app.comments[0].comment.body == "check this");
        let copied = app.copy.clone().expect("clipboard fallback");
        assert!(copied.contains("> src/main.rs L10:"));
        assert!(copied.contains(" let a = 1;"));
        assert!(app.toast.as_ref().is_some_and(|toast| toast.error));
    }

    #[test]
    fn escape_saves_the_comment_as_a_draft() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "later");
        press(&mut app, KeyCode::Esc);
        assert!(app.composer.is_none());
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.comments[0].comment.state, State::Draft);
        assert_eq!(app.comments[0].comment.anchor.start_line, 10);
    }

    #[test]
    fn github_posting_asks_first_and_returns_to_editing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c'));
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
    fn copying_a_range_carries_its_anchor_and_patch() {
        let mut app = app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('v'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('y'));
        let copied = app.copy.clone().expect("copied");
        assert!(copied.starts_with("> src/main.rs L11:\n\n```diff\n"));
        assert!(copied.contains("-let b = 2;\n+let b = 3;"));
        assert!(!copied.contains("why?"));
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
        press(&mut app, KeyCode::Char('C'));
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
    fn keys_skip_file_and_hunk_headings() {
        let mut app = app();
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.cursor, 3);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.cursor, 2);
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.cursor, 6);
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn a_range_of_only_removed_lines_anchors_to_the_old_side() {
        let mut app = app();
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('c'));
        let composer = app.composer.as_ref().expect("a comment box");
        assert_eq!(composer.anchor.side, Side::Old);
        assert_eq!(composer.anchor.start_line, 11);
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
            "> src/main.rs L11-L12:\n\n```diff\n+let b = 3;\n+let c = 4;\n```\n\nboth new\n"
        );
    }

    #[test]
    fn a_diff_without_changes_says_so() {
        let mut app = App::new(loaded(""), Options::default());
        let screen = draw(&mut app, 80, 10);
        assert!(screen.contains("nothing changed"));
    }
}
