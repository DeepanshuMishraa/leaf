//! `leaf pr`: the open pull requests and issues that concern you, and a pull
//! request read in full.
//!
//! The list has two tabs, Pull requests and Issues, each one list newest
//! first, its rows colored by whose they are and where. It shows the last
//! run's answers at once and refreshes behind them;
//! the row under the cursor is read ahead, so opening it is instant. A pull
//! request has a Summary (status, branch, reviewers, checks that need you,
//! the description with its images and videos) and a Timeline (commits,
//! comments, reviews); Comments holds the comments and reviews alone, to pick
//! and reply to; Code hands the diff, fetched once per head commit, to the
//! reviewer; Checks lists every check by workflow, shows a failed job's log,
//! and runs failed jobs again. Visible lists and pages refresh every ten seconds.
//! An issue has the same Summary, Timeline and Comments tabs. Text drags to
//! select and is copied when let go. It can be merged, closed, reopened,
//! marked ready, approved and commented on.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write as _};
use std::sync::LazyLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use dither_ratatui::dither_core::Field;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::art;
use crate::diff;
use crate::diffview::Leave;
use crate::look::Look;
use crate::news::News;
use crate::popover::{self, Edit};
use crate::pr::{
    self, Action, CheckStatus, CloseReason, Detail, Ended, Ending, Entry, EventKind, Kind,
    Mergeability, PrRef, Relation, Source, State, Status, Verdict,
};
use crate::prmedia::{self, Gallery, Shown};
use crate::render::{self, DIM, Figure};
use crate::usage;
use crate::video::PlayFailed;

const GREEN: Color = Color::Indexed(2);
const RED: Color = Color::Indexed(1);
const YELLOW: Color = Color::Indexed(3);
const PURPLE: Color = Color::Indexed(5);
const BLUE: Color = Color::Indexed(4);
/// Width of the label column in the summary.
const LABEL: usize = 11;
/// The widest the page gets; past this, lines are too long to read comfortably.
const PAGE_WIDTH: u16 = 96;
/// Rows a timeline comment shows before it's cut short, until `e` expands them.
const EXCERPT: usize = 12;
/// How many rows one wheel notch moves.
const WHEEL_ROWS: usize = 3;
/// A pull request read this recently isn't asked for again on opening.
const FRESH: Duration = Duration::from_secs(30);
/// How long the list's selection rests before that pull request is fetched.
const SETTLE: Duration = Duration::from_millis(150);
/// Beside a picked comment's time, when there's room.
const REPLY_HINT: &str = "enter replies";
/// The list's note while an `R` is still being answered.
const RELOADING: &str = "Already reloading";
/// How often the visible list or detail is read again.
const POLL: Duration = Duration::from_secs(10);
/// How often a running job's steps are read again while they're shown.
const JOB_POLL: Duration = Duration::from_secs(5);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// The keys an issue's footer offers, in the order they're dropped to fit.
const ISSUE_KEYS: [(&str, &str); 7] = [
    ("←→", "tabs"),
    ("c", "comment"),
    ("]", "reply"),
    ("x", "close"),
    ("o", "browser"),
    ("?", "keys"),
    ("q", "back"),
];
/// The keys the page's footer offers, in the order they're dropped to fit.
const KEYS: [(&str, &str); 8] = [
    ("←→", "tabs"),
    ("m", "merge"),
    ("c", "comment"),
    ("a", "approve"),
    ("x", "close"),
    ("o", "browser"),
    ("?", "keys"),
    ("q", "back"),
];
/// The footer's keys on the Checks tab.
const CHECK_KEYS: [(&str, &str); 7] = [
    ("←→", "tabs"),
    ("↑↓", "pick"),
    ("enter", "steps and log"),
    ("r", "re-run"),
    ("o", "open"),
    ("?", "keys"),
    ("q", "back"),
];
/// The footer's keys over a job's log.
const LOG_KEYS: [(&str, &str); 5] = [
    ("↑↓", "pick"),
    ("e", "open all groups"),
    ("r", "re-run"),
    ("o", "on GitHub"),
    ("esc", "back"),
];

/// The spinner's frame now.
fn spinner() -> char {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    SPINNER[(START.elapsed().as_millis() / 80) as usize % SPINNER.len()]
}

/// Where `leaf pr` starts.
pub enum Start {
    List,
    Pr(PrRef),
}

enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

enum Msg {
    Searched(usize, Result<Vec<Entry>, String>),
    /// Your GitHub login, which colors the list's rows.
    Me(String),
    Viewed(PrRef, Box<Result<Detail, String>>),
    Acted(PrRef, Kind, Result<String, String>),
    /// The patch fetched for a head commit.
    Patched(PrRef, String, Result<String, String>),
    /// An Actions job's steps and log.
    Logged(u64, Result<pr::JobLog, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Timeline,
    /// Only the comments and reviews, each in full, to pick and reply to.
    Comments,
    /// Every check on the pull request, and a job's steps and log.
    Checks,
}

impl Tab {
    /// The page's own tabs in order; a pull request's Code tab sits between
    /// Comments and Checks.
    const ALL: [Tab; 4] = [Tab::Summary, Tab::Timeline, Tab::Comments, Tab::Checks];

    /// Whether the tab draws pictures and videos: the description's, or comments'.
    fn shows_media(self) -> bool {
        matches!(self, Tab::Summary | Tab::Timeline | Tab::Comments)
    }
}

/// Where moving one tab on or back from the shown one lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Tab(Tab),
    /// A pull request's diff, which opens in the reviewer.
    Code,
    /// Past the first or last tab, with no wrapping asked for.
    Stay,
}

/// What a click in the tab row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabHit {
    Summary,
    Timeline,
    Comments,
    Code,
    Checks,
}

enum Mode {
    Normal,
    /// Picking how to merge.
    Merge,
    /// Picking why an issue is closed.
    Close,
    /// Picking which jobs to run again: each choice's key and action.
    Rerun(Vec<(char, Action)>),
    Confirm(Action),
    Compose {
        text: Vec<char>,
        cursor: usize,
        /// Whose comment this replies to, quoted at the top of `text`.
        reply: Option<String>,
        /// Why enter didn't send it, shown until the next key.
        refused: Option<&'static str>,
    },
    Help,
}

enum Fetch {
    /// Not asked yet this run: the done searches wait until they're shown.
    Idle,
    Loading,
    Done,
    Failed(String),
}

/// One search behind the list: what it found last, and whether it's asking again.
struct Slot {
    source: Source,
    status: Status,
    entries: Option<Vec<Entry>>,
    fetch: Fetch,
    background: bool,
}

/// Whose rows the list shows: everything that concerns you, or only those
/// assigned to you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Assigned,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::All => "All",
            Scope::Assigned => "Assigned",
        }
    }

    fn other(self) -> Scope {
        match self {
            Scope::All => Scope::Assigned,
            Scope::Assigned => Scope::All,
        }
    }

    fn holds(self, source: &Source) -> bool {
        match self {
            Scope::All => true,
            Scope::Assigned => matches!(source, Source::Assigned(_)),
        }
    }
}

/// What a click in the list's tab row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListHit {
    Kind(Kind),
    Status(Status),
    Scope(Scope),
}

struct List {
    /// The tab shown.
    kind: Kind,
    /// Open rows, or merged and closed ones.
    status: Status,
    /// Everything, or only what's assigned to you.
    scope: Scope,
    slots: Vec<Slot>,
    /// Your login, once known; until then rows aren't colored.
    me: Option<String>,
    /// Where each tab's and switch's label was drawn, for clicks.
    tab_hits: Vec<(Rect, ListHit)>,
    typed: String,
    selected: usize,
    /// The selected pull request and when it was picked, to fetch it once it rests there.
    resting: Option<(PrRef, Instant)>,
    /// Where each row was drawn, for clicks: its screen rows and its index.
    hits: Vec<(u16, u16, usize)>,
    /// The pull request a press on the picked row landed on, which opens on
    /// release unless the mouse was dragged or the list changed under it.
    pressed: Option<PrRef>,
    /// What ctrl+y or a drag copied, shown in the footer until the next key.
    note: Option<String>,
    /// The rows' lines as drawn last, and where, so a drag can select their text.
    shown: Vec<Line<'static>>,
    shown_area: Rect,
    /// Text dragged over in `shown`.
    selection: Option<Selection>,
    dragging: bool,
    polled: Instant,
    pending: Vec<(usize, Vec<Entry>)>,
}

impl List {
    /// The searches behind a tab, for the shown status and scope.
    fn slots_of(&self, kind: Kind) -> impl Iterator<Item = &Slot> {
        let (status, scope) = (self.status, self.scope);
        self.slots.iter().filter(move |s| {
            s.source.kind() == kind && s.status == status && scope.holds(&s.source)
        })
    }

    /// Everything a tab found for the shown status, each once, newest first.
    fn all(&self, kind: Kind) -> Vec<Entry> {
        pr::merge(self.slots_of(kind).filter_map(|s| s.entries.clone()))
    }

    /// The shown tab's rows that match the filter.
    fn entries(&self) -> Vec<Entry> {
        let filter = self.typed.to_lowercase();
        self.all(self.kind)
            .into_iter()
            .filter(|e| {
                filter.is_empty()
                    || e.title.to_lowercase().contains(&filter)
                    || e.pr.label().to_lowercase().contains(&filter)
                    || e.author.to_lowercase().contains(&filter)
            })
            .collect()
    }

    fn picked(&self) -> Option<PrRef> {
        self.entries().into_iter().nth(self.selected).map(|e| e.pr)
    }

    fn count(&self) -> usize {
        self.entries().len()
    }

    fn relation(&self, entry: &Entry) -> Relation {
        self.me
            .as_deref()
            .map_or(Relation::Other, |me| Relation::of(entry, me))
    }

    fn show(&mut self, kind: Kind) {
        if self.kind != kind {
            self.kind = kind;
            self.reset_pick();
        }
    }

    /// Switches between open rows and done ones, keeping the tab.
    fn show_status(&mut self, status: Status) {
        if self.status != status {
            self.status = status;
            self.reset_pick();
        }
    }

    /// Switches between everything and what's assigned to you.
    fn show_scope(&mut self, scope: Scope) {
        if self.scope != scope {
            self.scope = scope;
            self.reset_pick();
        }
    }

    fn reset_pick(&mut self) {
        self.selected = 0;
        self.resting = None;
        self.pressed = None;
        self.selection = None;
    }

    fn loading(&self) -> bool {
        self.slots_of(self.kind)
            .any(|s| matches!(s.fetch, Fetch::Loading))
    }

    fn reload(&mut self, shown_only: bool) -> Vec<(usize, Source, Status)> {
        self.polled = Instant::now();
        self.slots
            .iter_mut()
            .enumerate()
            .filter(|(_, slot)| {
                if shown_only {
                    slot.status == self.status && slot.source.kind() == self.kind
                } else {
                    slot.status == self.status || !matches!(slot.fetch, Fetch::Idle)
                }
            })
            .map(|(index, slot)| {
                slot.fetch = Fetch::Loading;
                slot.background = shown_only;
                (index, slot.source.clone(), slot.status)
            })
            .collect()
    }

    fn poll(&mut self) -> Vec<(usize, Source, Status)> {
        if self.dragging || self.selection.is_some() || !self.typed.is_empty() {
            return Vec::new();
        }
        for (index, entries) in std::mem::take(&mut self.pending) {
            self.replace_entries(index, entries);
        }
        // Every search `reload(true)` starts, whatever the scope shows, so a
        // slow one outside it isn't started again while it's still out.
        let (kind, status) = (self.kind, self.status);
        let in_flight = self.slots.iter().any(|s| {
            s.source.kind() == kind && s.status == status && matches!(s.fetch, Fetch::Loading)
        });
        if self.polled.elapsed() < POLL || in_flight {
            return Vec::new();
        }
        self.reload(true)
    }

    fn searched(&mut self, index: usize, result: Result<Vec<Entry>, String>) {
        let Some(slot) = self.slots.get_mut(index) else {
            return;
        };
        match result {
            Ok(entries) => {
                slot.fetch = Fetch::Done;
                self.pending.retain(|(at, _)| *at != index);
                if self.dragging
                    || self.selection.is_some()
                    || (slot.background && !self.typed.is_empty())
                {
                    self.pending.push((index, entries));
                } else {
                    self.replace_entries(index, entries);
                }
            }
            Err(error) => slot.fetch = Fetch::Failed(error),
        }
        if !self
            .slots
            .iter()
            .any(|slot| matches!(slot.fetch, Fetch::Loading))
            && self.note.as_deref() == Some(RELOADING)
        {
            self.note = None;
        }
    }

    fn replace_entries(&mut self, index: usize, entries: Vec<Entry>) {
        let picked = self.picked();
        if let Some(slot) = self.slots.get_mut(index) {
            slot.entries = Some(entries);
        }
        let entries = self.entries();
        self.selected = picked
            .and_then(|pr| entries.iter().position(|entry| entry.pr == pr))
            .unwrap_or(self.selected)
            .min(entries.len().saturating_sub(1));
    }

    fn failure(&self) -> Option<String> {
        self.slots_of(self.kind).find_map(|s| match &s.fetch {
            Fetch::Failed(e) => Some(e.clone()),
            _ => None,
        })
    }
}

/// A point in the body: its line, and the column in it.
type Point = (usize, u16);

/// Text dragged over in the body, from where the drag started to where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: Point,
    head: Point,
}

impl Selection {
    fn ordered(self) -> (Point, Point) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The columns selected on `line`, end exclusive.
    fn columns(self, line: usize) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if line < start.0 || line > end.0 {
            return None;
        }
        let from = if line == start.0 { start.1 } else { 0 };
        let to = if line == end.0 {
            end.1.saturating_add(1)
        } else {
            u16::MAX
        };
        Some((from, to))
    }

    /// The text of `lines` it covers, as drawn.
    fn text(self, lines: &[Line<'static>]) -> String {
        let out: Vec<String> = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let (from, to) = self.columns(index)?;
                Some(columns(&plain(line), from, to).trim_end().to_owned())
            })
            .collect();
        out.join("\n").trim().to_owned()
    }
}

/// The point under a screen cell in lines drawn in `area` from `scroll` on,
/// clamped into the area.
fn point_in(area: Rect, scroll: usize, column: u16, row: u16) -> Point {
    let y = row.clamp(area.y, (area.y + area.height).saturating_sub(1)) - area.y;
    (
        scroll + y as usize,
        column.saturating_sub(area.x).min(area.width),
    )
}

/// What a body was laid out for: width, tab, expanded, media generation, and
/// on the Checks tab, a stamp of what it shows.
type BodyKey = (u16, Tab, bool, u64, u64);

/// A tab's lines, and the figures drawn over them.
struct Body {
    lines: Vec<Line<'static>>,
    figures: Vec<Figure>,
}

struct Page {
    pr: PrRef,
    kind: Kind,
    detail: Load<Detail>,
    pending: Option<Detail>,
    tab: Tab,
    /// The timeline event picked to reply to, by its index.
    focus: Option<usize>,
    /// The timeline's first line of each event, as laid out last.
    starts: Vec<usize>,
    /// Scroll the picked event into view on the next draw.
    reveal: bool,
    scroll: usize,
    mode: Mode,
    /// What the footer says until the next key.
    toast: Option<String>,
    /// An action on its way to GitHub.
    busy: Option<String>,
    /// Waiting for the diff, to open the reviewer when it lands.
    code: bool,
    expanded: bool,
    selection: Option<Selection>,
    dragging: bool,
    cache: Option<(BodyKey, Body)>,
    /// Where the body was drawn last, for the mouse.
    body_area: Rect,
    /// The pinned header's lines and where they were drawn: selectable
    /// too, as the lines before the body's.
    header: Vec<Line<'static>>,
    header_area: Rect,
    tab_hits: Vec<(Rect, TabHit)>,
    /// Where the header shows the pull request's number; a click copies it.
    number_hit: Option<Rect>,
    /// The check picked on the Checks tab, in the order it lists them.
    check: usize,
    /// Scroll the picked check, or the log's last error, into view on the next draw.
    follow: bool,
    /// The body lines the Checks tab drew each check on, and its index.
    check_rows: Vec<(usize, usize)>,
    /// The Actions job whose steps and log the Checks tab shows instead of the list.
    log: Option<u64>,
    /// The step or group picked in the job, and those opened.
    pick: Fold,
    open_steps: HashSet<usize>,
    open_groups: HashSet<(usize, usize)>,
    /// Whether the job's failed or running step was opened once it arrived.
    log_init: bool,
    /// Scroll to the opened step's last error on the next draw.
    jump: bool,
    /// The body line each step's and shown group's header is on, and the
    /// line of each step's last error.
    fold_rows: Vec<(usize, Fold)>,
    step_errors: HashMap<usize, usize>,
    /// When the shown job was last read.
    log_polled: Instant,
    /// When the page last asked again, including failed attempts.
    polled: Instant,
}

impl Page {
    fn new(pr: PrRef, kind: Kind, detail: Load<Detail>) -> Self {
        Page {
            pr,
            kind,
            detail,
            pending: None,
            tab: Tab::Summary,
            focus: None,
            starts: Vec::new(),
            reveal: false,
            scroll: 0,
            mode: Mode::Normal,
            toast: None,
            busy: None,
            code: false,
            expanded: false,
            selection: None,
            dragging: false,
            cache: None,
            body_area: Rect::default(),
            header: Vec::new(),
            header_area: Rect::default(),
            tab_hits: Vec::new(),
            number_hit: None,
            check: 0,
            follow: false,
            check_rows: Vec::new(),
            log: None,
            pick: Fold::Step(0),
            open_steps: HashSet::new(),
            open_groups: HashSet::new(),
            log_init: false,
            jump: false,
            fold_rows: Vec::new(),
            step_errors: HashMap::new(),
            log_polled: Instant::now(),
            polled: Instant::now(),
        }
    }

    fn show(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.scroll = 0;
            self.selection = None;
            self.cache = None;
            self.follow = true;
        }
        // Comments opens on a comment picked, so enter replies at once.
        // With none to pick, the tab says so itself, so no toast.
        if tab == Tab::Comments && self.focus.is_none() {
            let toast = self.toast.take();
            self.pick(1);
            self.toast = toast;
        }
    }

    /// The tab `step` places on (1) or back (-1) from the shown one. A pull
    /// request's Code tab comes between Comments and Checks; `wrap` goes round
    /// past the ends, as tab and shift-tab do.
    fn step(&self, step: isize, wrap: bool) -> Step {
        let mut places: Vec<Step> = Tab::ALL.iter().map(|tab| Step::Tab(*tab)).collect();
        match self.kind {
            // Code sits between Comments and Checks.
            Kind::Pull => places.insert(3, Step::Code),
            // An issue has no Code or Checks tab.
            Kind::Issue => places.truncate(3),
        }
        let at = places
            .iter()
            .position(|place| *place == Step::Tab(self.tab))
            .unwrap_or(0) as isize;
        let count = places.len() as isize;
        let next = at + step;
        let next = if wrap {
            next.rem_euclid(count)
        } else if (0..count).contains(&next) {
            next
        } else {
            return Step::Stay;
        };
        places[next as usize]
    }

    /// The link to this pull request or issue.
    fn url(&self) -> String {
        match self.kind {
            Kind::Pull => self.pr.url(),
            Kind::Issue => self.pr.issue_url(),
        }
    }

    /// Picks the next (`step` 1) or previous (-1) comment on the timeline to reply to.
    fn pick(&mut self, step: isize) {
        let Load::Ready(detail) = &self.detail else {
            return;
        };
        let replyable: Vec<usize> = detail
            .timeline
            .iter()
            .enumerate()
            .filter(|(_, e)| quoted(e).is_some())
            .map(|(i, _)| i)
            .collect();
        let next = match (self.focus, step > 0) {
            (None, true) => replyable.first(),
            (None, false) => replyable.last(),
            (Some(at), true) => replyable.iter().find(|i| **i > at).or(replyable.last()),
            (Some(at), false) => replyable
                .iter()
                .rev()
                .find(|i| **i < at)
                .or(replyable.first()),
        };
        if let Some(&next) = next {
            if self.tab != Tab::Comments {
                self.show(Tab::Timeline);
            }
            self.focus = Some(next);
            self.reveal = true;
            self.cache = None;
        } else {
            self.toast = Some("No comments to reply to yet".to_owned());
        }
    }

    /// The composer, holding the picked comment quoted.
    fn reply(&mut self) {
        let Load::Ready(detail) = &self.detail else {
            return;
        };
        let Some(event) = self.focus.and_then(|i| detail.timeline.get(i)) else {
            self.toast = Some("Press ] or [ to pick a comment first".to_owned());
            return;
        };
        let Some(quote) = quoted(event) else { return };
        let text: Vec<char> = quote.chars().collect();
        self.mode = Mode::Compose {
            cursor: text.len(),
            text,
            reply: Some(event.actor.clone()),
            refused: None,
        };
    }

    fn refreshable(&self) -> bool {
        matches!(self.mode, Mode::Normal)
            && self.selection.is_none()
            && !self.dragging
            && self.busy.is_none()
    }

    /// Shows the pull request read again, and returns the job to read when
    /// the open log's check now runs as another job.
    fn refreshed(&mut self, detail: Detail) -> Option<u64> {
        if matches!(self.detail, Load::Ready(_)) && !self.refreshable() {
            self.pending = Some(detail);
            return None;
        }
        self.pending = None;
        let focus = match &self.detail {
            Load::Ready(old) => self.focus.and_then(|at| {
                let event = old.timeline.get(at)?;
                (old.timeline
                    .iter()
                    .filter(|other| event.same_identity(other))
                    .count()
                    == 1)
                    .then_some(event)
            }),
            _ => None,
        };
        let focus = focus.and_then(|event| {
            let mut matches = detail
                .timeline
                .iter()
                .enumerate()
                .filter(|(_, next)| event.same_identity(next));
            let (at, _) = matches.next()?;
            matches.next().is_none().then_some(at)
        });
        if focus != self.focus {
            self.reveal = focus.is_some();
        }
        self.focus = focus;
        // A refresh can reorder the checks; the picked one stays picked.
        let picked = self.picked_check().map(pr::Check::label);
        let found = picked.and_then(|label| {
            check_order(&detail.checks)
                .iter()
                .position(|&i| detail.checks[i].label() == label)
        });
        if let Some(at) = found {
            self.check = at;
        }
        self.detail = Load::Ready(detail);
        self.cache = None;
        // The status and the log describe the same run of the job; a check
        // that's gone closes its log rather than handing it to another.
        let shown = self.log?;
        let now = found.and_then(|_| job_after_refresh(self.picked_check()));
        if now == Some(shown) {
            return None;
        }
        self.log = now;
        self.log_init = false;
        self.open_steps.clear();
        self.open_groups.clear();
        self.pick = Fold::Step(0);
        self.scroll = 0;
        self.follow = true;
        now
    }

    /// Opens a step or group, or closes it.
    fn toggle(&mut self, fold: Fold) {
        match fold {
            Fold::Step(step) => {
                if !self.open_steps.remove(&step) {
                    self.open_steps.insert(step);
                }
            }
            Fold::Group(step, group) => {
                if !self.open_groups.remove(&(step, group)) {
                    self.open_groups.insert((step, group));
                }
            }
        }
    }

    /// The check picked on the Checks tab.
    fn picked_check(&self) -> Option<&pr::Check> {
        let Load::Ready(detail) = &self.detail else {
            return None;
        };
        let index = *check_order(&detail.checks).get(self.check)?;
        detail.checks.get(index)
    }

    fn head(&self) -> String {
        match &self.detail {
            Load::Ready(detail) => detail.head_sha.clone(),
            _ => String::new(),
        }
    }

    /// The point under a screen cell in the page's lines: the header's,
    /// then the body's, which start after them. Clamped into the body below it.
    fn point(&self, column: u16, row: u16) -> Point {
        let header = self.header_area;
        if row < self.body_area.y && row >= header.y && !header.is_empty() {
            return point_in(header, 0, column, row);
        }
        let (line, col) = point_in(self.body_area, self.scroll, column, row);
        (self.header.len() + line, col)
    }

    /// The body line a page point is on, or `None` in the header.
    fn body_line(&self, point: Point) -> Option<usize> {
        point.0.checked_sub(self.header.len())
    }

    fn selected_text(&self) -> String {
        let Some(selection) = self.selection else {
            return String::new();
        };
        let body = self
            .cache
            .as_ref()
            .map_or(&[][..], |(_, body)| &body.lines[..]);
        let lines: Vec<Line<'static>> = self.header.iter().chain(body).cloned().collect();
        selection.text(&lines)
    }
}

/// A row of a job that opens and closes: a step, or a group in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Fold {
    Step(usize),
    Group(usize, usize),
}

impl Fold {
    fn step(self) -> usize {
        match self {
            Fold::Step(step) | Fold::Group(step, _) => step,
        }
    }
}

/// Actions jobs' steps and logs, by job id.
#[derive(Default)]
struct Logs {
    jobs: HashMap<u64, Load<pr::JobLog>>,
    inflight: HashSet<u64>,
    /// Bumped whenever a job arrives, so the Checks tab is laid out again.
    generation: u64,
}

struct App {
    tx: Sender<Msg>,
    /// The theme and the UI setting, followed while the view is open.
    look: Look,
    /// This version's notes, up once after an update until OK.
    news: Option<News>,
    /// Time spent here, for the home screen's heat map.
    clock: usage::Clock,
    rx: Receiver<Msg>,
    here: Option<String>,
    branch: Option<String>,
    /// `None` when leaf was started on one pull request; leaving it quits.
    list: Option<List>,
    page: Option<Page>,
    /// Pull requests read this run, and when.
    details: HashMap<PrRef, (Detail, Instant)>,
    /// Pull requests being read now.
    inflight: HashSet<PrRef>,
    /// Pull requests the list fetched ahead; each is tried once.
    prefetched: HashSet<PrRef>,
    /// Each pull request's patch, and the head commit it's of.
    patches: HashMap<PrRef, (String, String)>,
    patching: HashSet<PrRef>,
    logs: Logs,
    gallery: Gallery,
    now: i64,
}

/// Runs the pull request viewer until it's closed.
pub fn run(start: Start, here: Option<String>, branch: Option<String>) -> io::Result<()> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    let setup = execute!(io::stdout(), EnableMouseCapture, SetTitle("leaf pr"));
    // Asked once the terminal is raw, as the reader does.
    let gallery = Gallery::detect();
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        look: Look::detect(),
        news: News::unseen(),
        clock: usage::Clock::start(),
        tx,
        rx,
        here,
        branch,
        list: None,
        page: None,
        details: HashMap::new(),
        inflight: HashSet::new(),
        prefetched: HashSet::new(),
        patches: HashMap::new(),
        patching: HashSet::new(),
        logs: Logs::default(),
        gallery,
        now: pr::now(),
    };
    match start {
        Start::List => app.open_list(),
        Start::Pr(pr) => app.open_page(pr, Kind::Pull),
    }
    let result = setup.and_then(|_| app.event_loop(&mut terminal));
    let _ = app.gallery.clear();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

impl App {
    /// Shows the list from the last run's answers and asks the open searches
    /// again, all at once.
    fn open_list(&mut self) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            if let Ok(me) = pr::viewer() {
                let _ = tx.send(Msg::Me(me));
            }
        });
        // Every search twice over: open rows asked now, done ones shown from
        // last time and asked only once they're looked at.
        let sources = pr::sources(self.here.as_deref(), self.branch.as_deref());
        let slots: Vec<Slot> = [Status::Open, Status::Done]
            .into_iter()
            .flat_map(|status| {
                sources.iter().map(move |source| Slot {
                    entries: pr::cached_search(source, status),
                    source: source.clone(),
                    status,
                    fetch: Fetch::Idle,
                    background: false,
                })
            })
            .collect();
        self.list = Some(List {
            kind: Kind::Pull,
            status: Status::Open,
            scope: Scope::All,
            slots,
            me: pr::cached_viewer(),
            tab_hits: Vec::new(),
            typed: String::new(),
            selected: 0,
            resting: None,
            hits: Vec::new(),
            pressed: None,
            note: None,
            shown: Vec::new(),
            shown_area: Rect::default(),
            selection: None,
            dragging: false,
            polled: Instant::now(),
            pending: Vec::new(),
        });
        self.ask_shown();
    }

    /// Asks the searches behind the shown status that haven't been asked
    /// this run, for both tabs, so flipping tabs finds them ready.
    fn ask_shown(&mut self) {
        let Some(list) = &mut self.list else {
            return;
        };
        let status = list.status;
        let asks: Vec<(usize, Source)> = list
            .slots
            .iter_mut()
            .enumerate()
            .filter(|(_, s)| s.status == status && matches!(s.fetch, Fetch::Idle))
            .map(|(index, slot)| {
                slot.fetch = Fetch::Loading;
                (index, slot.source.clone())
            })
            .collect();
        for (index, source) in asks {
            self.search(index, &source, status);
        }
    }

    /// `tab` on the list: open rows or done ones.
    fn flip_status(&mut self) {
        if let Some(list) = &mut self.list {
            list.show_status(list.status.other());
        }
        self.ask_shown();
    }

    /// Asks search `index` again; the answer arrives as `Msg::Searched`.
    fn search(&self, index: usize, source: &Source, status: Status) {
        let (tx, asked) = (self.tx.clone(), source.clone());
        std::thread::spawn(move || {
            let result = pr::search(&asked, status).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Searched(index, result));
        });
    }

    /// `R` on the list: asks again now every search asked this run and those
    /// behind what's shown, keeping the rows shown until the answers arrive,
    /// and lets rows be read ahead afresh.
    fn reload_list(&mut self) {
        let Some(list) = &mut self.list else {
            return;
        };
        if list.slots.iter().any(|s| matches!(s.fetch, Fetch::Loading)) {
            list.note = Some(RELOADING.to_owned());
            return;
        }
        let asks = list.reload(false);
        // The tab row's spinner shows it's asking.
        self.prefetched.clear();
        for (index, source, status) in asks {
            self.search(index, &source, status);
        }
    }

    /// Opens a pull request on what's known of it, and reads it again unless that's fresh.
    fn open_page(&mut self, pr: PrRef, kind: Kind) {
        let known = self
            .details
            .get(&pr)
            .map(|(detail, at)| {
                let mut detail = detail.clone();
                // Signed upload links expire within minutes; the fresh read brings new ones.
                if at.elapsed() >= prmedia::SIGNED_LINKS_LAST {
                    detail.media.clear();
                }
                detail
            })
            .or_else(|| pr::cached_view(&pr, kind));
        let detail = match known {
            Some(detail) => {
                if kind == Kind::Pull {
                    let head = detail.head_sha.clone();
                    self.want_patch(&pr, &head);
                }
                Load::Ready(detail)
            }
            None => Load::Loading,
        };
        self.page = Some(Page::new(pr.clone(), kind, detail));
        self.fetch(pr, kind, false);
    }

    fn fetch(&mut self, pr: PrRef, kind: Kind, force: bool) {
        let fresh = self
            .details
            .get(&pr)
            .is_some_and(|(_, at)| at.elapsed() < FRESH);
        if self.inflight.contains(&pr) || (fresh && !force) {
            return;
        }
        self.inflight.insert(pr.clone());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = pr::view(&pr, kind).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Viewed(pr, Box::new(result)));
        });
    }

    /// Makes the patch at `head` ready: kept from before, read from the
    /// cache, or fetched in the background.
    fn want_patch(&mut self, pr: &PrRef, head: &str) {
        if self
            .patches
            .get(pr)
            .is_some_and(|(at, _)| head.is_empty() || at == head)
        {
            return;
        }
        if let Some(patch) = pr::cached_patch(pr, head) {
            self.patches.insert(pr.clone(), (head.to_owned(), patch));
            return;
        }
        if !self.patching.insert(pr.clone()) {
            return;
        }
        let (tx, pr, head) = (self.tx.clone(), pr.clone(), head.to_owned());
        std::thread::spawn(move || {
            let result = pr::fetch_patch(&pr, &head);
            let _ = tx.send(Msg::Patched(pr, head, result));
        });
    }

    /// Reads the selected pull request ahead once the selection rests on it.
    fn prefetch(&mut self) {
        if self.page.is_some() {
            return;
        }
        let Some(list) = &mut self.list else { return };
        let kind = list.kind;
        let Some(picked) = list.picked() else { return };
        match &list.resting {
            Some((pr, at)) if *pr == picked => {
                if at.elapsed() >= SETTLE
                    && !self.details.contains_key(&picked)
                    && self.prefetched.insert(picked.clone())
                {
                    self.fetch(picked, kind, false);
                }
            }
            _ => list.resting = Some((picked, Instant::now())),
        }
    }

    fn act(&mut self, action: Action) {
        let Some(page) = &mut self.page else { return };
        page.mode = Mode::Normal;
        page.busy = Some(match &action {
            Action::Merge(method) => format!("{}…", method.label()),
            Action::Close => "Closing…".to_owned(),
            Action::CloseAs(reason) => format!("Closing as {}…", reason.label()),
            Action::Reopen => "Reopening…".to_owned(),
            Action::Ready => "Marking ready…".to_owned(),
            Action::Draft => "Converting to draft…".to_owned(),
            Action::Approve => "Approving…".to_owned(),
            Action::Comment(_) => "Posting comment…".to_owned(),
            Action::Rerun { scope, .. } => format!("{}…", scope.label()),
        });
        // The job shown is replaced by a new attempt; the list shows it running.
        if matches!(action, Action::Rerun { .. }) {
            page.log = None;
            page.follow = true;
        }
        let (tx, pr, kind) = (self.tx.clone(), page.pr.clone(), page.kind);
        std::thread::spawn(move || {
            let result = pr::perform(&pr, kind, &action).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Acted(pr, kind, result));
        });
    }

    /// Shows the picked check's steps and log, reading them unless they're here.
    fn open_log(&mut self) {
        let Some(page) = &mut self.page else { return };
        let Some(check) = page.picked_check() else {
            return;
        };
        let Some(job) = check.job_id else {
            page.toast = Some(format!(
                "{} isn't a GitHub Actions job, so its log is on its own site. o opens it",
                check.label()
            ));
            return;
        };
        page.log = Some(job);
        page.pick = Fold::Step(0);
        page.open_steps.clear();
        page.open_groups.clear();
        page.log_init = false;
        page.scroll = 0;
        page.selection = None;
        let settled = matches!(self.logs.jobs.get(&job), Some(Load::Ready(log)) if log.settled());
        if !settled {
            self.read_job(job);
        }
    }

    /// Reads a job's steps and log, keeping what's shown of it meanwhile.
    fn read_job(&mut self, job: u64) {
        let Some(page) = &mut self.page else { return };
        page.log_polled = Instant::now();
        if !self.logs.inflight.insert(job) {
            return;
        }
        if !matches!(self.logs.jobs.get(&job), Some(Load::Ready(_))) {
            self.logs.jobs.insert(job, Load::Loading);
        }
        let (tx, repo) = (self.tx.clone(), page.pr.repo.clone());
        std::thread::spawn(move || {
            let result = pr::job_log(&repo, job).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Logged(job, result));
        });
    }

    fn poll(&mut self) {
        let Some(page) = &mut self.page else {
            if let Some(list) = &mut self.list {
                let asks = list.poll();
                for (index, source, status) in asks {
                    self.search(index, &source, status);
                }
            }
            return;
        };
        if !page.refreshable() || !matches!(page.detail, Load::Ready(_)) {
            return;
        }
        let rejob = page
            .pending
            .take()
            .and_then(|detail| page.refreshed(detail));
        let read = self
            .details
            .get(&page.pr)
            .is_none_or(|(_, at)| at.elapsed() >= POLL);
        let job = rejob.or_else(|| {
            page.log.filter(|job| {
                page.tab == Tab::Checks
                    // Still running, or its log failed to download: GitHub's
                    // log endpoint often answers 5xx for a moment after a job ends.
                    && matches!(self.logs.jobs.get(job), Some(Load::Ready(log)) if !log.settled())
                    && page.log_polled.elapsed() >= JOB_POLL
            })
        });
        if read && page.polled.elapsed() >= POLL {
            page.polled = Instant::now();
            let (pr, kind) = (page.pr.clone(), page.kind);
            self.fetch(pr, kind, true);
        }
        if let Some(job) = job {
            self.read_job(job);
        }
    }

    /// The Code tab: the reviewer at once when the patch is here, else when it lands.
    fn open_code(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let Some(page) = &mut self.page else {
            return Ok(());
        };
        // An issue has no diff, so there is nothing to open.
        if page.kind == Kind::Issue {
            return Ok(());
        }
        let (pr, head) = (page.pr.clone(), page.head());
        self.want_patch(&pr, &head);
        match self.patches.get(&pr) {
            Some((_, patch)) => {
                let patch = patch.clone();
                self.review(&pr, patch, terminal)
            }
            None => {
                if let Some(page) = &mut self.page {
                    page.code = true;
                }
                Ok(())
            }
        }
    }

    fn receive(&mut self, msg: Msg, terminal: &mut DefaultTerminal) -> io::Result<()> {
        match msg {
            Msg::Searched(index, result) => {
                if let Some(list) = &mut self.list {
                    list.searched(index, result);
                }
            }
            Msg::Me(me) => {
                if let Some(list) = &mut self.list {
                    list.me = Some(me);
                }
            }
            Msg::Viewed(pr, result) => {
                self.inflight.remove(&pr);
                let shown = self.page.as_ref().is_some_and(|p| p.pr == pr);
                match *result {
                    Ok(detail) => {
                        if shown && detail.kind == Kind::Pull {
                            // The diff is fetched while the page is read, so Code opens at once.
                            let head = detail.head_sha.clone();
                            self.want_patch(&pr, &head);
                        }
                        let rejob = self
                            .page
                            .as_mut()
                            .filter(|p| p.pr == pr)
                            .and_then(|page| page.refreshed(detail.clone()));
                        self.details.insert(pr, (detail, Instant::now()));
                        if let Some(job) = rejob {
                            self.read_job(job);
                        }
                    }
                    Err(e) => {
                        if let Some(page) = self.page.as_mut().filter(|p| p.pr == pr) {
                            // A failed refresh keeps what was shown.
                            if matches!(page.detail, Load::Ready(_)) {
                                page.toast = Some(e);
                            } else {
                                page.detail = Load::Failed(e);
                            }
                        }
                    }
                }
            }
            Msg::Acted(pr, kind, result) => {
                if let Some(page) = self.page.as_mut().filter(|p| p.pr == pr) {
                    page.busy = None;
                    page.toast = Some(match result {
                        Ok(done) => done,
                        Err(e) => e,
                    });
                }
                self.fetch(pr, kind, true);
            }
            Msg::Logged(job, result) => {
                self.logs.inflight.remove(&job);
                self.logs.generation += 1;
                match result {
                    Ok(log) => {
                        self.logs.jobs.insert(job, Load::Ready(log));
                    }
                    // A failed refresh keeps what was shown.
                    Err(e) => match self.logs.jobs.get(&job) {
                        Some(Load::Ready(_)) => {
                            if let Some(page) = self.page.as_mut().filter(|p| p.log == Some(job)) {
                                page.toast = Some(e);
                            }
                        }
                        _ => {
                            self.logs.jobs.insert(job, Load::Failed(e));
                        }
                    },
                }
            }
            Msg::Patched(pr, head, result) => {
                self.patching.remove(&pr);
                let waiting = self.page.as_ref().is_some_and(|p| p.pr == pr && p.code);
                match result {
                    Ok(patch) => {
                        self.patches.insert(pr.clone(), (head, patch.clone()));
                        if waiting {
                            if let Some(page) = &mut self.page {
                                page.code = false;
                            }
                            self.review(&pr, patch, terminal)?;
                        }
                    }
                    Err(e) => {
                        if let Some(page) = self.page.as_mut().filter(|_| waiting) {
                            page.code = false;
                            page.toast = Some(e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Hands the terminal to the diff reviewer, then takes it back on the tab it asked for.
    fn review(
        &mut self,
        pr: &PrRef,
        patch: String,
        terminal: &mut DefaultTerminal,
    ) -> io::Result<()> {
        let request = diff::Request {
            origin: diff::Origin::Github { url: pr.url() },
            repo: None,
            paths: Vec::new(),
        };
        let loaded = request.github(patch, pr.resource());
        let failure = std::rc::Rc::new(std::cell::RefCell::new(None));
        let mut options = crate::diff_options(&loaded.key, failure.clone());
        options.tabs = true;
        self.gallery.stop();
        self.gallery.clear()?;
        let _ = execute!(io::stdout(), DisableMouseCapture);
        ratatui::restore();
        let result = crate::diffview::run(loaded, options);
        *terminal = ratatui::init();
        terminal.clear()?;
        self.gallery.reset();
        execute!(io::stdout(), EnableMouseCapture, SetTitle("leaf pr"))?;
        if let Some(page) = &mut self.page {
            match &result {
                Ok(Leave::Summary) => page.show(Tab::Summary),
                Ok(Leave::Timeline) => page.show(Tab::Timeline),
                Ok(Leave::Comments) => page.show(Tab::Comments),
                Ok(Leave::Checks) => page.show(Tab::Checks),
                Ok(Leave::Quit) | Err(_) => {}
            }
            page.toast = match (result, failure.borrow().as_ref()) {
                (Err(e), _) => Some(format!("The diff reviewer failed ({e})")),
                (Ok(_), Some(message)) => Some(message.clone()),
                (Ok(_), None) => None,
            };
        }
        Ok(())
    }

    /// Whether anything on screen is waiting, so the spinner needs frames.
    fn animating(&self) -> bool {
        match &self.page {
            Some(page) => {
                matches!(page.detail, Load::Loading)
                    || page.busy.is_some()
                    || page.code
                    || self.inflight.contains(&page.pr)
                    || (page.tab.shows_media() && self.gallery.loading())
                    || page
                        .log
                        .is_some_and(|job| self.logs.inflight.contains(&job))
            }
            None => self
                .list
                .as_ref()
                .is_some_and(|l| l.loading() || l.resting.is_some()),
        }
    }

    fn event_loop(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            while let Ok(msg) = self.rx.try_recv() {
                self.receive(msg, terminal)?;
            }
            self.gallery.receive();
            // A video plays only while a tab that can show it is on screen.
            if self.page.as_ref().is_none_or(|p| !p.tab.shows_media()) {
                self.gallery.stop();
            }
            if let Some(failed) = self.gallery.tick()
                && let Some(page) = &mut self.page
            {
                page.toast = Some(failed);
            }
            self.prefetch();
            self.poll();
            self.now = pr::now();
            // Following the theme reads a reply from stdin, so only with no key waiting.
            let idle = !event::poll(Duration::ZERO)?;
            self.look.refresh(idle);
            self.gallery.begin();
            terminal.draw(|f| self.draw(f))?;
            self.gallery.flush()?;
            // Drawn again when the next thing on screen changes: a video or
            // GIF frame, the loading ring, a spinner, or the half-second clock.
            let idle = if self.animating() {
                Duration::from_millis(80)
            } else {
                Duration::from_millis(500)
            };
            let wait = self
                .gallery
                .next_redraw()
                .map_or(idle, |next| next.clamp(Duration::from_millis(5), idle));
            // Wakes in time to follow the theme and the UI setting.
            let wait = wait.min(self.look.due());
            if !event::poll(wait)? {
                continue;
            }
            self.clock.touch();
            match event::read()? {
                Event::Key(key) if News::key(&mut self.news, key) => {}
                Event::Mouse(mouse) if News::mouse(&mut self.news, mouse) => {}
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if !self.key(key, terminal)? {
                        return Ok(());
                    }
                }
                Event::Mouse(mouse) => self.mouse(mouse, terminal)?,
                // The redraw after a resize clears the screen, and the images with it.
                Event::Resize(..) => self.gallery.reset(),
                _ => {}
            }
        }
    }

    fn mouse(&mut self, mouse: MouseEvent, terminal: &mut DefaultTerminal) -> io::Result<()> {
        let Some(page) = &mut self.page else {
            self.list_mouse(mouse);
            return Ok(());
        };
        let inside = |rect: Rect| {
            mouse.column >= rect.x
                && mouse.column < rect.x + rect.width
                && mouse.row >= rect.y
                && mouse.row < rect.y + rect.height
        };
        match mouse.kind {
            MouseEventKind::ScrollDown => page.scroll += WHEEL_ROWS,
            MouseEventKind::ScrollUp => page.scroll = page.scroll.saturating_sub(WHEEL_ROWS),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, hit)) = page.tab_hits.iter().find(|(rect, _)| inside(*rect)) {
                    match hit {
                        TabHit::Summary => page.show(Tab::Summary),
                        TabHit::Timeline => page.show(Tab::Timeline),
                        TabHit::Comments => page.show(Tab::Comments),
                        TabHit::Code => return self.open_code(terminal),
                        TabHit::Checks => page.show(Tab::Checks),
                    }
                    return Ok(());
                }
                if page.number_hit.is_some_and(inside) {
                    let label = page.pr.label();
                    copy(&label);
                    page.toast = Some(format!("Copied {label}"));
                    return Ok(());
                }
                if !inside(page.body_area) && !inside(page.header_area) {
                    page.selection = None;
                    return Ok(());
                }
                let point = page.point(mouse.column, mouse.row);
                let line = page.body_line(point);
                // A click on a check picks it.
                if page.tab == Tab::Checks
                    && page.log.is_none()
                    && let Some(&(_, index)) =
                        line.and_then(|line| page.check_rows.iter().find(|(row, _)| *row == line))
                {
                    page.check = index;
                }
                // A click on a step opens or closes it.
                if page.tab == Tab::Checks
                    && page.log.is_some()
                    && let Some(&(_, fold)) =
                        line.and_then(|line| page.fold_rows.iter().find(|(row, _)| *row == line))
                {
                    page.pick = fold;
                    page.toggle(fold);
                    return Ok(());
                }
                // A click on a video (or its label below) plays it here, or
                // stops it; a click on an image shows it full size in the browser.
                let figure = page.cache.as_ref().zip(line).and_then(|((_, body), line)| {
                    body.figures
                        .iter()
                        .find(|f| line >= f.top && line <= f.top + f.rows as usize)
                        .map(|f| (f.href.clone(), f.cols))
                });
                if let Some((href, cols)) = figure {
                    page.selection = None;
                    let gallery = &mut self.gallery;
                    page.toast = if gallery.is_playing(&href) {
                        gallery.stop();
                        None
                    } else if gallery.is_video(&href) {
                        let width = u32::from(cols) * u32::from(gallery.cell().0);
                        match gallery.play(&href, width) {
                            Ok(()) => None,
                            // Without ffmpeg the browser plays it instead.
                            Err(e @ PlayFailed::NoFfmpeg) => Some(match prmedia::open(&href) {
                                Ok(()) => format!("{e}, so it opened in the browser"),
                                Err(open) => format!("{e}, and the browser didn't open ({open})"),
                            }),
                            Err(e) => Some(e.to_string()),
                        }
                    } else {
                        Some(match prmedia::open(&href) {
                            Ok(()) => "Opened it in the browser".to_owned(),
                            Err(e) => format!("Couldn't open {href} ({e})"),
                        })
                    };
                    return Ok(());
                }
                page.selection = Some(Selection {
                    anchor: point,
                    head: point,
                });
                page.dragging = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if page.dragging => {
                // Dragging past the body's edge scrolls it; once it's at the top,
                // a drag upward goes on into the header.
                let area = page.body_area;
                let from_body = page
                    .selection
                    .is_some_and(|s| page.body_line(s.anchor).is_some());
                let scrolling_up = mouse.row < area.y && from_body && page.scroll > 0;
                if scrolling_up {
                    page.scroll = page.scroll.saturating_sub(1);
                } else if mouse.row >= area.y + area.height {
                    page.scroll += 1;
                }
                // While the body still scrolls, the drag holds at its top row.
                let row = if scrolling_up { area.y } else { mouse.row };
                let point = page.point(mouse.column, row);
                if let Some(selection) = &mut page.selection {
                    selection.head = point;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if page.dragging => {
                page.dragging = false;
                match page.selection {
                    Some(s) if s.anchor != s.head => {
                        let text = page.selected_text();
                        if !text.is_empty() {
                            copy(&text);
                            page.toast =
                                Some(format!("Copied {} characters", text.chars().count()));
                        }
                    }
                    _ => page.selection = None,
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Opens a row's pull request or issue.
    fn choose(&mut self, pr: PrRef) {
        let Some(list) = &self.list else { return };
        let kind = list.kind;
        self.open_page(pr, kind);
    }

    fn list_mouse(&mut self, mouse: MouseEvent) {
        let Some(list) = &mut self.list else { return };
        let last = list.count().saturating_sub(1);
        let point = point_in(list.shown_area, 0, mouse.column, mouse.row);
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(&(_, hit)) = list.tab_hits.iter().find(|(rect, _)| {
                mouse.row == rect.y && (rect.x..rect.x + rect.width).contains(&mouse.column)
            })
        {
            list.selection = None;
            match hit {
                ListHit::Kind(kind) => list.show(kind),
                ListHit::Scope(scope) => list.show_scope(scope),
                ListHit::Status(status) => {
                    list.show_status(status);
                    self.ask_shown();
                }
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                list.selection = None;
                list.selected = (list.selected + 1).min(last);
            }
            MouseEventKind::ScrollUp => {
                list.selection = None;
                list.selected = list.selected.saturating_sub(1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A drag from here selects the text it passes over.
                list.selection = Some(Selection {
                    anchor: point,
                    head: point,
                });
                list.dragging = true;
                let Some(&(_, _, index)) = list
                    .hits
                    .iter()
                    .find(|(top, bottom, _)| (*top..=*bottom).contains(&mouse.row))
                else {
                    return;
                };
                // A click picks a row; a click on the picked row opens it once
                // let go, so a drag (to select its number) never opens it.
                list.pressed = if index == list.selected {
                    list.picked()
                } else {
                    None
                };
                list.selected = index;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                list.pressed = None;
                if list.dragging
                    && let Some(selection) = &mut list.selection
                {
                    selection.head = point;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if std::mem::take(&mut list.dragging) {
                    match list.selection {
                        Some(s) if s.anchor != s.head => {
                            let text = s.text(&list.shown);
                            if !text.is_empty() {
                                copy(&text);
                                list.note =
                                    Some(format!("Copied {} characters", text.chars().count()));
                            }
                        }
                        _ => list.selection = None,
                    }
                }
                // Still the same pull request: a search can reorder rows mid-click.
                if let Some(pr) = list.pressed.take()
                    && list.picked().as_ref() == Some(&pr)
                {
                    self.choose(pr);
                }
            }
            _ => {}
        }
    }

    /// Handles a key; `false` closes the viewer.
    fn key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) -> io::Result<bool> {
        if self.page.is_some() {
            return self.page_key(key, terminal);
        }
        let Some(list) = &mut self.list else {
            return Ok(false);
        };
        let rows = list.count();
        let picked = list.picked();
        list.note = None;
        list.selection = None;
        match key.code {
            // Two tabs, so either arrow flips between them.
            KeyCode::Left | KeyCode::Right => {
                list.show(match list.kind {
                    Kind::Pull => Kind::Issue,
                    Kind::Issue => Kind::Pull,
                });
                return Ok(true);
            }
            // Tab flips open rows and done ones: merged or closed.
            KeyCode::Tab | KeyCode::BackTab => {
                self.flip_status();
                return Ok(true);
            }
            // `@` for "@me": everything, or only what's assigned to you.
            KeyCode::Char('@') => {
                list.show_scope(list.scope.other());
                return Ok(true);
            }
            // Asks every search again now rather than waiting. `R` only while
            // nothing's typed, so a filter can still hold an R (it ignores
            // case, so `r` starts one); ctrl+r works either way.
            KeyCode::Char('R') if list.typed.is_empty() => {
                self.reload_list();
                return Ok(true);
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reload_list();
                return Ok(true);
            }
            // Letters filter the list, so copying the number takes ctrl.
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(pr) = &picked {
                    let number = format!("#{}", pr.number);
                    copy(&number);
                    list.note = Some(format!("Copied {number}"));
                }
                return Ok(true);
            }
            KeyCode::PageDown => {
                list.selected = (list.selected + 8).min(rows.saturating_sub(1));
                return Ok(true);
            }
            KeyCode::PageUp => {
                list.selected = list.selected.saturating_sub(8);
                return Ok(true);
            }
            _ => {}
        }
        match popover::edit(
            key.code,
            key.modifiers,
            &mut list.typed,
            &mut list.selected,
            rows,
        ) {
            Edit::Quit if !list.typed.is_empty() && key.code == KeyCode::Esc => {
                list.typed.clear();
                list.selected = 0;
            }
            Edit::Quit => return Ok(false),
            Edit::Choose => {
                if let Some(pr) = picked {
                    self.choose(pr);
                }
            }
            Edit::Complete | Edit::Typed | Edit::Moved | Edit::Nothing => {}
        }
        Ok(true)
    }

    fn page_key(&mut self, key: KeyEvent, terminal: &mut DefaultTerminal) -> io::Result<bool> {
        let Some(page) = &mut self.page else {
            return Ok(true);
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Ok(false);
        }
        page.toast = None;
        let detail = match &page.detail {
            Load::Ready(detail) => Some(detail),
            _ => None,
        };

        match &mut page.mode {
            Mode::Help => {
                page.mode = Mode::Normal;
                return Ok(true);
            }
            Mode::Merge => {
                let methods = detail.map(|d| d.methods.clone()).unwrap_or_default();
                page.mode = match key.code {
                    KeyCode::Char(c) => match methods.iter().find(|m| m.key() == c) {
                        Some(method) => Mode::Confirm(Action::Merge(*method)),
                        None => Mode::Merge,
                    },
                    KeyCode::Enter => match methods.first() {
                        Some(method) => Mode::Confirm(Action::Merge(*method)),
                        None => Mode::Normal,
                    },
                    _ => Mode::Normal,
                };
                return Ok(true);
            }
            Mode::Close => {
                let picked = match key.code {
                    KeyCode::Char(c) => CloseReason::ALL.into_iter().find(|r| r.key() == c),
                    KeyCode::Enter => Some(CloseReason::Completed),
                    _ => None,
                };
                match picked {
                    Some(reason) => self.act(Action::CloseAs(reason)),
                    None => page.mode = Mode::Normal,
                }
                return Ok(true);
            }
            Mode::Rerun(choices) => {
                page.mode = match key.code {
                    KeyCode::Char(c) => match choices.iter().find(|(key, _)| *key == c) {
                        Some((_, action)) => Mode::Confirm(action.clone()),
                        None => Mode::Normal,
                    },
                    _ => Mode::Normal,
                };
                return Ok(true);
            }
            Mode::Confirm(action) => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Char('y')) {
                    let action = action.clone();
                    self.act(action);
                } else {
                    page.mode = Mode::Normal;
                }
                return Ok(true);
            }
            Mode::Compose {
                text,
                cursor,
                reply,
                refused,
            } => {
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                *refused = None;
                match key.code {
                    KeyCode::Esc => page.mode = Mode::Normal,
                    KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                        text.insert(*cursor, '\n');
                        *cursor += 1;
                    }
                    KeyCode::Enter => {
                        let body: String = text.iter().collect::<String>().trim().to_owned();
                        match empty_comment(&body, reply.is_some()) {
                            Some(why) => *refused = Some(why),
                            None => self.act(Action::Comment(body)),
                        }
                    }
                    KeyCode::Backspace if *cursor > 0 => {
                        *cursor -= 1;
                        text.remove(*cursor);
                    }
                    KeyCode::Left => *cursor = cursor.saturating_sub(1),
                    KeyCode::Right => *cursor = (*cursor + 1).min(text.len()),
                    KeyCode::Char('u') if ctrl => {
                        text.drain(..*cursor);
                        *cursor = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        text.insert(*cursor, c);
                        *cursor += 1;
                    }
                    _ => {}
                }
                return Ok(true);
            }
            Mode::Normal => {}
        }

        let half = terminal.size().map(|s| s.height as usize / 2).unwrap_or(10);
        if page.tab == Tab::Checks {
            let count = detail.map_or(0, |d| d.checks.len());
            let at = page.fold_rows.iter().position(|(_, f)| *f == page.pick);
            let fold = |index: usize| page.fold_rows.get(index).map(|(_, f)| *f);
            match key.code {
                // Escape lets go of a selection first; q and backspace go back at once.
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace
                    if page.log.is_some()
                        && (key.code != KeyCode::Esc || page.selection.is_none()) =>
                {
                    page.selection = None;
                    page.log = None;
                    page.scroll = 0;
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Char('j') | KeyCode::Down if page.log.is_some() => {
                    let next = at.map_or(0, |at| at + 1);
                    page.pick = fold(next).unwrap_or(page.pick);
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Char('k') | KeyCode::Up if page.log.is_some() => {
                    let next = at.map_or(0, |at| at.saturating_sub(1));
                    page.pick = fold(next).unwrap_or(page.pick);
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Enter | KeyCode::Char(' ') if page.log.is_some() => {
                    page.toggle(page.pick);
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Char('j') | KeyCode::Down if page.log.is_none() => {
                    page.check = (page.check + 1).min(count.saturating_sub(1));
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Char('k') | KeyCode::Up if page.log.is_none() => {
                    page.check = page.check.saturating_sub(1);
                    page.follow = true;
                    return Ok(true);
                }
                KeyCode::Enter if page.log.is_none() => {
                    self.open_log();
                    return Ok(true);
                }
                KeyCode::Char('R') if page.log.is_some() => {
                    if let Some(job) = page.log {
                        self.read_job(job);
                    }
                    return Ok(true);
                }
                KeyCode::Char('o') => {
                    if let Some(url) = page.picked_check().and_then(|c| c.url.clone()) {
                        page.toast = Some(match prmedia::open(&url) {
                            Ok(()) => "Opened it in the browser".to_owned(),
                            Err(e) => format!("Couldn't open {url} ({e})"),
                        });
                        return Ok(true);
                    }
                }
                // On this tab, r re-runs jobs rather than marking the pull request ready.
                KeyCode::Char('r') => {
                    page.toast = Some(match (page.busy.is_some(), detail) {
                        (true, _) => "Wait for the last action to finish".to_owned(),
                        (false, None) => "Still loading".to_owned(),
                        (false, Some(detail)) => match reruns(detail, page.picked_check()) {
                            Ok(choices) => {
                                page.mode = Mode::Rerun(choices);
                                return Ok(true);
                            }
                            Err(why) => why,
                        },
                    });
                    return Ok(true);
                }
                _ => {}
            }
        }
        match key.code {
            // Escape lets go of a selection before it leaves.
            KeyCode::Esc if self.gallery.any_playing() => self.gallery.stop(),
            KeyCode::Esc if page.selection.is_some() => page.selection = None,
            KeyCode::Esc if page.focus.is_some() => {
                page.focus = None;
                page.cache = None;
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.page = None;
                if self.list.is_none() {
                    return Ok(false);
                }
            }
            // On Comments, j and k move between comments rather than lines.
            KeyCode::Char('j') | KeyCode::Down if page.tab == Tab::Comments => page.pick(1),
            KeyCode::Char('k') | KeyCode::Up if page.tab == Tab::Comments => page.pick(-1),
            KeyCode::Char('j') | KeyCode::Down => page.scroll += 1,
            KeyCode::Char('k') | KeyCode::Up => page.scroll = page.scroll.saturating_sub(1),
            KeyCode::Char('d') if ctrl => page.scroll += half,
            KeyCode::Char('u') if ctrl => page.scroll = page.scroll.saturating_sub(half),
            KeyCode::Char(' ') | KeyCode::PageDown => page.scroll += half * 2,
            KeyCode::Char('b') | KeyCode::PageUp => {
                page.scroll = page.scroll.saturating_sub(half * 2)
            }
            KeyCode::Char('g') | KeyCode::Home => page.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => page.scroll = usize::MAX,
            KeyCode::Char(']') => page.pick(1),
            KeyCode::Char('[') => page.pick(-1),
            KeyCode::Enter if page.busy.is_none() && page.focus.is_some() => page.reply(),
            // Tab cycles Summary → Timeline → Comments → Code → Checks and
            // round; shift-tab goes the other way. The arrows stop at the ends.
            KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h' | 'l') => {
                let forward =
                    matches!(key.code, KeyCode::Tab | KeyCode::Right | KeyCode::Char('l'));
                let wrap = matches!(key.code, KeyCode::Tab | KeyCode::BackTab);
                match page.step(if forward { 1 } else { -1 }, wrap) {
                    Step::Tab(tab) => page.show(tab),
                    Step::Code => self.open_code(terminal)?,
                    Step::Stay => {}
                }
            }
            KeyCode::Char('1') => page.show(Tab::Summary),
            KeyCode::Char('2') => page.show(Tab::Timeline),
            KeyCode::Char('3') => page.show(Tab::Comments),
            KeyCode::Char('4') if page.kind == Kind::Pull => self.open_code(terminal)?,
            KeyCode::Char('5') if page.kind == Kind::Pull => page.show(Tab::Checks),
            KeyCode::Char('e') => {
                page.expanded = !page.expanded;
                page.cache = None;
            }
            KeyCode::Char('?') => page.mode = Mode::Help,
            KeyCode::Char('R') => {
                let (pr, kind) = (page.pr.clone(), page.kind);
                self.fetch(pr, kind, true);
            }
            KeyCode::Char('o') => {
                let opened = match page.kind {
                    Kind::Pull => pr::open_in_browser(&page.pr),
                    Kind::Issue => pr::open_issue(&page.pr),
                };
                page.toast = Some(match opened {
                    Ok(()) => format!("Opened {} in the browser", page.pr.label()),
                    Err(e) => e.to_string(),
                });
            }
            KeyCode::Char('y') if page.kind == Kind::Pull => {
                copy(&page.pr.checkout());
                page.toast = Some(format!("Copied {}", page.pr.checkout()));
            }
            KeyCode::Char('y' | 'Y') => {
                let url = page.url();
                copy(&url);
                page.toast = Some(format!("Copied {url}"));
            }
            KeyCode::Char('n') => {
                let number = format!("#{}", page.pr.number);
                copy(&number);
                page.toast = Some(format!("Copied {number}"));
            }
            KeyCode::Char('c') if page.busy.is_none() => {
                page.mode = Mode::Compose {
                    text: Vec::new(),
                    cursor: 0,
                    reply: None,
                    refused: None,
                };
            }
            KeyCode::Char(c @ ('m' | 'x' | 'r' | 'a')) => {
                if page.busy.is_some() {
                    page.toast = Some("Wait for the last action to finish".to_owned());
                    return Ok(true);
                }
                let Some(detail) = detail else {
                    page.toast = Some("Still loading".to_owned());
                    return Ok(true);
                };
                match available(detail, c) {
                    Ok(Next::Merge) => page.mode = Mode::Merge,
                    Ok(Next::Close) => page.mode = Mode::Close,
                    Ok(Next::Confirm(action)) => page.mode = Mode::Confirm(action),
                    Ok(Next::Now(action)) => self.act(action),
                    Err(why) => page.toast = Some(why),
                }
            }
            _ => {}
        }
        Ok(true)
    }

    fn draw(&mut self, f: &mut Frame) {
        let now = self.now;
        let ink = self.look.ink();
        if let Some(page) = &mut self.page {
            let refreshing = self.inflight.contains(&page.pr);
            draw_page(f, page, &mut self.gallery, &self.logs, now, refreshing, ink);
        } else if let Some(list) = &mut self.list {
            draw_list(f, list, now, ink);
        }
        if let Some(news) = &mut self.news {
            let panel = news.draw(f, ink);
            // Kitty images sit above the text, so any under the dialog would hide it.
            self.gallery.uncover(panel);
        }
    }
}

/// What a key leads to on a pull request in this state.
enum Next {
    Merge,
    /// Pick why the issue is closed.
    Close,
    Confirm(Action),
    Now(Action),
}

fn available(detail: &Detail, key: char) -> Result<Next, String> {
    let open = detail.state.is_open();
    let state = || {
        format!(
            "#{} is {}",
            detail.pr.number,
            detail.state.label().to_lowercase()
        )
    };
    if detail.kind == Kind::Issue {
        return match key {
            'x' if open => Ok(Next::Close),
            'x' => Ok(Next::Confirm(Action::Reopen)),
            'm' | 'r' | 'a' => {
                Err("Only pull requests can be merged, drafted or approved".to_owned())
            }
            _ => Err(String::new()),
        };
    }
    match key {
        'm' => {
            if !open {
                return Err(state());
            }
            if !detail.can_write {
                return Err(format!("You don't have write access to {}", detail.pr.repo));
            }
            if detail.state == State::Draft {
                return Err("Drafts can't be merged. Press r to mark it ready first".to_owned());
            }
            if detail.mergeability == Mergeability::Conflicts {
                return Err(format!(
                    "This branch conflicts with {}. Resolve the conflicts first",
                    detail.base
                ));
            }
            if detail.methods.is_empty() {
                return Err(format!("{} allows no merge methods", detail.pr.repo));
            }
            Ok(Next::Merge)
        }
        'x' => match detail.state {
            State::Merged => Err(state()),
            State::Closed | State::Completed | State::NotPlanned => {
                Ok(Next::Confirm(Action::Reopen))
            }
            State::Open | State::Draft => Ok(Next::Confirm(Action::Close)),
        },
        'r' => match detail.state {
            State::Draft => Ok(Next::Now(Action::Ready)),
            State::Open => Ok(Next::Confirm(Action::Draft)),
            State::Merged | State::Closed | State::Completed | State::NotPlanned => Err(state()),
        },
        'a' if open => Ok(Next::Confirm(Action::Approve)),
        'a' => Err(state()),
        _ => Err(String::new()),
    }
}

/// The job to show once the pull request is read again: the picked check's
/// job as it is now, since a push or a re-run replaces it with a new one.
fn job_after_refresh(check: Option<&pr::Check>) -> Option<u64> {
    check.and_then(|c| c.job_id)
}

/// What GitHub lets you re-run for the picked check, with the keys that
/// pick each, or why there's nothing.
fn reruns(detail: &Detail, check: Option<&pr::Check>) -> Result<Vec<(char, Action)>, String> {
    let Some(check) = check else {
        return Err("No check is picked".to_owned());
    };
    let Some(run) = check.run_id else {
        return Err(format!(
            "{} isn't a GitHub Actions job, so it can't be re-run from here",
            check.label()
        ));
    };
    if !detail.can_write {
        return Err(format!("You don't have write access to {}", detail.pr.repo));
    }
    let in_run = |c: &&pr::Check| c.run_id == Some(run);
    if detail
        .checks
        .iter()
        .filter(in_run)
        .any(|c| c.status == CheckStatus::Running)
    {
        return Err(format!(
            "{} is still running; GitHub re-runs a run once it's done",
            check.workflow
        ));
    }
    let workflow = if check.workflow.is_empty() {
        check.name.clone()
    } else {
        check.workflow.clone()
    };
    let mut choices = Vec::new();
    if detail
        .checks
        .iter()
        .filter(in_run)
        .any(|c| c.status == CheckStatus::Failed)
    {
        choices.push((
            'f',
            Action::Rerun {
                run,
                scope: pr::Rerun::Failed,
                name: workflow.clone(),
            },
        ));
    }
    if let Some(job) = check.job_id {
        choices.push((
            'j',
            Action::Rerun {
                run,
                scope: pr::Rerun::Job(job),
                name: check.name.clone(),
            },
        ));
    }
    choices.push((
        'a',
        Action::Rerun {
            run,
            scope: pr::Rerun::All,
            name: workflow,
        },
    ));
    Ok(choices)
}

fn copy(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07").and_then(|_| out.flush());
}

fn state_color(state: State) -> Color {
    match state {
        State::Open => GREEN,
        State::Draft => DIM,
        State::Merged | State::Completed => PURPLE,
        State::Closed => RED,
        State::NotPlanned => DIM,
    }
}

fn check_glyph(status: CheckStatus) -> (&'static str, Color) {
    match status {
        CheckStatus::Passed => ("✓", GREEN),
        CheckStatus::Failed => ("✗", RED),
        CheckStatus::Running => ("●", YELLOW),
        CheckStatus::Skipped => ("–", DIM),
    }
}

fn verdict_glyph(verdict: Verdict) -> (&'static str, Color) {
    match verdict {
        Verdict::Approved => ("✓", GREEN),
        Verdict::ChangesRequested => ("✗", RED),
        Verdict::Requested => ("●", YELLOW),
        Verdict::Commented => ("○", DIM),
        Verdict::Dismissed => ("–", DIM),
    }
}

fn relation_color(relation: Relation) -> Color {
    match relation {
        Relation::Yours => GREEN,
        Relation::Outgoing => BLUE,
        Relation::Incoming => YELLOW,
        Relation::Other => DIM,
    }
}

/// The list: the tabs and a legend, a filter, then the shown tab's rows
/// newest first on two lines, title above and who and when below, in one
/// centered column.
fn draw_list(f: &mut Frame, list: &mut List, now: i64, ink: Option<art::Ink>) {
    let area = f.area();
    let (x, width) = column(area);
    let w = width as usize;
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let entries = list.entries();
    let total = entries.len();
    list.selected = list.selected.min(total.saturating_sub(1));

    // The tabs, each with how many it has, and a spinner while asking again.
    let row = area.y + 1;
    let mut tabs = Vec::new();
    list.tab_hits.clear();
    let mut at = x;
    for kind in [Kind::Pull, Kind::Issue] {
        let on = kind == list.kind;
        let label = kind.title();
        let count = list.all(kind).len();
        let width = label.width() as u16;
        list.tab_hits
            .push((Rect::new(at, row, width, 1), ListHit::Kind(kind)));
        tabs.push(Span::styled(label, if on { bold } else { dim }));
        let count = if count > 0 {
            format!(" {count}")
        } else {
            String::new()
        };
        at += width + count.width() as u16 + 4;
        tabs.push(Span::styled(count, dim));
        tabs.push(Span::raw("    "));
    }
    if list.loading() {
        tabs.push(Span::styled(spinner().to_string(), dim));
    }
    // Two switches at the right, each the shown side bright and underlined,
    // the other quiet: whose rows (`@`), then open or done ones (tab). A click
    // flips either.
    let mut switch = Vec::new();
    let mut labels = Vec::new();
    for scope in [Scope::All, Scope::Assigned] {
        if !switch.is_empty() {
            switch.push(Span::styled("  ·  ", dim));
        }
        let style = if scope == list.scope {
            bold.add_modifier(Modifier::UNDERLINED)
        } else {
            dim
        };
        labels.push((
            switch.iter().map(|s| s.content.width()).sum::<usize>(),
            scope.label(),
            ListHit::Scope(scope),
        ));
        switch.push(Span::styled(scope.label(), style));
    }
    switch.push(Span::raw("     "));
    for status in [Status::Open, Status::Done] {
        let label = status.label(list.kind);
        let on = status == list.status;
        let style = if on {
            bold.add_modifier(Modifier::UNDERLINED).fg(match status {
                Status::Open => GREEN,
                Status::Done => PURPLE,
            })
        } else {
            dim
        };
        if status == Status::Done {
            switch.push(Span::styled("  ·  ", dim));
        }
        labels.push((
            switch.iter().map(|s| s.content.width()).sum::<usize>(),
            label,
            ListHit::Status(status),
        ));
        switch.push(Span::styled(label, style));
    }
    let switch_width: usize = switch.iter().map(|s| s.content.width()).sum();
    let switch_x = x + width.saturating_sub(switch_width as u16);
    for (offset, label, hit) in labels {
        list.tab_hits.push((
            Rect::new(switch_x + offset as u16, row, label.width() as u16, 1),
            hit,
        ));
    }
    f.render_widget(
        Paragraph::new(spread(tabs, switch, w)),
        Rect::new(x, row, width, 1),
    );

    // Beside the filter: what the colors mean for open rows; for done ones,
    // a sparkline of the last twelve weeks.
    let mut aside = Vec::new();
    match list.status {
        Status::Open if list.me.is_some() => {
            for relation in Relation::ALL {
                if entries.iter().any(|e| list.relation(e) == relation) {
                    if !aside.is_empty() {
                        aside.push(Span::raw("   "));
                    }
                    aside.push(Span::styled(
                        "● ",
                        Style::default().fg(relation_color(relation)),
                    ));
                    aside.push(Span::styled(relation.label(), dim));
                }
            }
        }
        Status::Open => {}
        Status::Done => {
            let weeks = weekly(&list.all(list.kind), now, SPARK_WEEKS);
            let most = weeks.iter().copied().max().unwrap_or(0);
            if most > 0 {
                for count in &weeks {
                    let (bar, style) = match count {
                        0 => ("▁", dim),
                        n => (
                            SPARK[(n * (SPARK.len() - 1)).div_ceil(most)],
                            Style::default().fg(PURPLE),
                        ),
                    };
                    aside.push(Span::styled(bar, style));
                }
                let sum: usize = weeks.iter().sum();
                let verb = match list.kind {
                    Kind::Pull => "merged",
                    Kind::Issue => "closed",
                };
                aside.push(Span::styled(
                    format!("  {sum} {verb} in {SPARK_WEEKS} weeks"),
                    dim,
                ));
            }
        }
    }
    let field = if list.typed.is_empty() {
        vec![
            Span::styled("/  ", dim),
            Span::styled("▏", bold),
            Span::styled("Filter by title, repository, number or author", dim),
        ]
    } else {
        vec![
            Span::styled("/  ", dim),
            Span::raw(list.typed.clone()),
            Span::styled("▏", bold),
        ]
    };
    let used: usize = field.iter().chain(&aside).map(|s| s.content.width()).sum();
    if used + 2 >= w {
        aside.clear();
    }
    f.render_widget(
        Paragraph::new(spread(field, aside, w)),
        Rect::new(x, area.y + 3, width, 1),
    );

    let top = area.y + 5;
    let bottom = (area.y + area.height).saturating_sub(2);
    let room = bottom.saturating_sub(top) as usize;

    if total == 0 {
        list.shown.clear();
        list.selection = None;
        let what = list.kind.title().to_lowercase();
        let state = list.status.label(list.kind).to_lowercase();
        let text = if list.loading() {
            format!("{}  Loading your {state} {what}", spinner())
        } else if let Some(e) = list.failure().filter(|_| list.typed.is_empty()) {
            e
        } else if list.typed.is_empty() && list.scope == Scope::Assigned {
            format!("No {state} {what} are assigned to you.")
        } else if list.typed.is_empty() {
            format!("No {state} {what} concern you.")
        } else {
            format!("Nothing matches “{}”.", list.typed)
        };
        if let (true, Some(ink)) = (list.loading(), ink) {
            let field = Rect::new(area.x, top + 2, area.width, room.saturating_sub(2) as u16);
            art::field(f.buffer_mut(), field, ink, Field::Ripple, art::clock());
        }
        let lines: Vec<Line> = wrap(&text, w).into_iter().map(Line::from).collect();
        f.render_widget(
            Paragraph::new(lines).style(dim),
            Rect::new(x, top + 1, width, room.saturating_sub(1) as u16),
        );
    } else {
        // Every line of the list, and the row index each belongs to.
        let mut lines: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        let mut group = None;
        for (index, entry) in entries.iter().enumerate() {
            // Done rows fall under when they ended: Today, Yesterday, …
            if let Some(ended) = entry.ended.as_ref().filter(|_| list.status == Status::Done) {
                let here = age_group(&ended.at, now);
                if group != Some(here) {
                    if group.is_some() {
                        lines.push((Line::default(), None));
                    }
                    group = Some(here);
                    let count = entries
                        .iter()
                        .filter(|e| e.ended.as_ref().map(|e| age_group(&e.at, now)) == Some(here))
                        .count();
                    lines.push((group_heading(here, count, w), None));
                }
            }
            let picked = index == list.selected;
            let bar = if picked { "▌ " } else { "  " };
            let (glyph, color) = match (&entry.ended, list.status) {
                (Some(ended), Status::Done) => ending_glyph(ended.how),
                _ => (
                    if entry.draft { "○" } else { "●" },
                    relation_color(list.relation(entry)),
                ),
            };
            let name = entry.pr.repo.rsplit('/').next().unwrap_or(&entry.pr.repo);
            lines.push((
                spread(
                    vec![
                        Span::styled(bar, bold),
                        Span::styled(format!("{glyph}  "), Style::default().fg(color)),
                        Span::styled(
                            entry.title.clone(),
                            if picked { bold } else { Style::default() },
                        ),
                    ],
                    vec![Span::styled(format!("{name} #{}", entry.pr.number), dim)],
                    w,
                ),
                Some(index),
            ));
            let about = match (&entry.ended, list.status) {
                // The author in their relation's color, so whose it was
                // still reads without the open list's dot.
                (Some(Ended { how, at }), Status::Done) => vec![
                    Span::raw("     "),
                    Span::styled(
                        entry.author.clone(),
                        Style::default().fg(relation_color(list.relation(entry))),
                    ),
                    Span::styled(format!(" · {} {}", how.verb(), pr::ago(at, now)), dim),
                ],
                _ => {
                    let mut about = format!("{} · {}", entry.author, pr::ago(&entry.updated, now));
                    if entry.draft {
                        about.push_str(" · draft");
                    }
                    vec![Span::raw("     "), Span::styled(about, dim)]
                }
            };
            lines.push((Line::from(about), Some(index)));
        }
        // Scroll so the picked row stays in view.
        let first_of = |wanted: usize| lines.iter().position(|(_, i)| *i == Some(wanted));
        let at = first_of(list.selected).unwrap_or(0);
        // A group's heading stays in view above its first row.
        let from = match at.checked_sub(1).map(|above| &lines[above]) {
            Some((_, None)) => at - 1,
            _ => at,
        };
        let skip = (at + 2).saturating_sub(room).min(from);
        list.hits.clear();
        let mut shown = Vec::new();
        for (y, (line, owner)) in lines.into_iter().skip(skip).take(room).enumerate() {
            if let Some(owner) = owner {
                let row = top + y as u16;
                match list.hits.last_mut() {
                    Some(hit) if hit.2 == owner => hit.1 = row,
                    _ => list.hits.push((row, row, owner)),
                }
            }
            shown.push(line);
        }
        let drawn: Vec<Line> = shown
            .iter()
            .enumerate()
            .map(
                |(index, line)| match list.selection.and_then(|s| s.columns(index)) {
                    Some((from, to)) => highlight(line, from, to),
                    None => line.clone(),
                },
            )
            .collect();
        list.shown = shown;
        list.shown_area = Rect::new(x, top, width, room as u16);
        f.render_widget(Paragraph::new(drawn), list.shown_area);
    }

    let flip = list.status.other().label(list.kind).to_lowercase();
    let scope = list.scope.other().label().to_lowercase();
    let footer = match (list.failure(), &list.note) {
        (_, Some(note)) => Line::from(Span::styled(clip(note, w), dim)),
        (Some(e), None) if total > 0 => Line::from(Span::styled(clip(&e, w), dim)),
        _ => keys_line(
            &[
                ("↑↓", "move"),
                ("enter", "open"),
                ("tab", &flip),
                ("@", &scope),
                ("←→", "pulls · issues"),
                ("type", "filter"),
                ("^y", "copy #"),
                ("R", "reload"),
                ("esc", "quit"),
            ],
            w,
        ),
    };
    if let Some(ink) = ink {
        art::chrome(f.buffer_mut(), area, x, width, 4, ink);
    }
    f.render_widget(
        Paragraph::new(footer),
        Rect::new(x, (area.y + area.height).saturating_sub(1), width, 1),
    );
}

/// How many weeks the done list's sparkline covers, and its bars.
const SPARK_WEEKS: usize = 12;
const SPARK: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// How many rows ended in each of the last `weeks` weeks, oldest first;
/// the last is the seven days up to `now`.
fn weekly(entries: &[Entry], now: i64, weeks: usize) -> Vec<usize> {
    const WEEK: i64 = 7 * 86_400;
    let mut counts = vec![0; weeks];
    for at in entries
        .iter()
        .filter_map(|e| e.ended.as_ref().and_then(|e| pr::seconds(&e.at)))
    {
        let ago = (now - at).max(0) / WEEK;
        if let Some(slot) = usize::try_from(ago)
            .ok()
            .and_then(|ago| weeks.checked_sub(ago + 1))
        {
            counts[slot] += 1;
        }
    }
    counts
}

/// The done list's groups, by how long ago a row ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgeGroup {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    ThisYear,
    Older,
}

impl AgeGroup {
    fn label(self) -> &'static str {
        match self {
            AgeGroup::Today => "Today",
            AgeGroup::Yesterday => "Yesterday",
            AgeGroup::ThisWeek => "This week",
            AgeGroup::ThisMonth => "This month",
            AgeGroup::ThisYear => "This year",
            AgeGroup::Older => "Older",
        }
    }
}

/// Which group a time falls in, counting whole days (UTC) back from `now`.
fn age_group(iso: &str, now: i64) -> AgeGroup {
    let Some(then) = pr::seconds(iso) else {
        return AgeGroup::Older;
    };
    match now.div_euclid(86_400) - then.div_euclid(86_400) {
        ..=0 => AgeGroup::Today,
        1 => AgeGroup::Yesterday,
        2..=6 => AgeGroup::ThisWeek,
        7..=30 => AgeGroup::ThisMonth,
        31..=364 => AgeGroup::ThisYear,
        _ => AgeGroup::Older,
    }
}

/// A group's heading: its name, a quiet rule, and how many rows it holds.
fn group_heading(group: AgeGroup, count: usize, width: usize) -> Line<'static> {
    let label = group.label();
    let count = count.to_string();
    let rule = width.saturating_sub(label.width() + count.width() + 4);
    Line::from(vec![
        Span::styled(
            format!("  {label} "),
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(rule), Style::default().fg(DIM)),
        Span::styled(format!(" {count}"), Style::default().fg(DIM)),
    ])
}

/// How a done row ended, as its glyph and color: merged and completed in
/// purple, as GitHub shows them; not planned quiet.
fn ending_glyph(how: Ending) -> (&'static str, Color) {
    match how {
        Ending::Merged => ("◆", PURPLE),
        Ending::Completed => ("✓", PURPLE),
        Ending::NotPlanned => ("⊘", DIM),
    }
}

/// Key hints, as many as fit: the key, then what it does, quieter.
fn keys_line(keys: &[(&str, &str)], width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, what) in keys {
        let gap = if spans.is_empty() { 0 } else { 3 };
        let needed = gap + key.width() + 1 + what.width();
        if used + needed > width {
            break;
        }
        if gap > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::raw((*key).to_owned()));
        spans.push(Span::styled(format!(" {what}"), Style::default().fg(DIM)));
        used += needed;
    }
    Line::from(spans)
}

/// `left` and `right` on one line `width` wide, `left` cut short to make room.
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let right_width: usize = right.iter().map(|s| s.content.width()).sum();
    let room = width.saturating_sub(right_width + 1);
    let mut spans = Vec::new();
    let mut used = 0;
    for span in left {
        let w = span.content.width();
        if used + w <= room {
            used += w;
            spans.push(span);
        } else {
            let cut = clip(&span.content, room.saturating_sub(used));
            used += cut.width();
            spans.push(Span::styled(cut, span.style));
            break;
        }
    }
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + right_width)),
    ));
    spans.extend(right);
    Line::from(spans)
}

/// The start of `text` that fits `room` columns, with an ellipsis when cut.
fn clip(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in text.chars() {
        used += c.width().unwrap_or(0);
        if used > room {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// A line's text, as drawn.
fn plain(line: &Line) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The part of `text` drawn in columns `from..to`.
fn columns(text: &str, from: u16, to: u16) -> String {
    let mut col = 0u16;
    let mut out = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0) as u16;
        if col >= from && col < to {
            out.push(c);
        }
        col = col.saturating_add(w);
    }
    out
}

/// `line` with columns `from..to` reversed, to show them selected.
fn highlight(line: &Line<'static>, from: u16, to: u16) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut col = 0u16;
    for span in &line.spans {
        let mut run = String::new();
        let mut run_selected = None;
        for c in span.content.chars() {
            let selected = col >= from && col < to;
            if run_selected.is_some_and(|s| s != selected) {
                let style = if run_selected == Some(true) {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_selected = Some(selected);
            run.push(c);
            col = col.saturating_add(c.width().unwrap_or(0) as u16);
        }
        if !run.is_empty() {
            let style = if run_selected == Some(true) {
                span.style.add_modifier(Modifier::REVERSED)
            } else {
                span.style
            };
            spans.push(Span::styled(run, style));
        }
    }
    // A selection past the line's end takes in the line break, as the reader shows it.
    if to == u16::MAX && from <= col {
        spans.push(Span::styled(
            " ",
            Style::default().add_modifier(Modifier::REVERSED),
        ));
    }
    Line::from(spans)
}

/// The page's column: at most `PAGE_WIDTH` wide, centered, with a margin.
fn column(area: Rect) -> (u16, u16) {
    let width = area.width.saturating_sub(6).min(PAGE_WIDTH);
    (area.x + (area.width - width) / 2, width)
}

fn draw_page(
    f: &mut Frame,
    page: &mut Page,
    gallery: &mut Gallery,
    logs: &Logs,
    now: i64,
    refreshing: bool,
    ink: Option<art::Ink>,
) {
    let area = f.area();
    let (x, width) = column(area);
    let w = width as usize;
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let detail = match &page.detail {
        Load::Ready(detail) => detail,
        Load::Loading | Load::Failed(_) => {
            page.header.clear();
            page.header_area = Rect::default();
            let lines: Vec<Line> = match &page.detail {
                Load::Failed(e) => wrap(e, w).into_iter().map(Line::from).collect(),
                _ => vec![Line::from(vec![
                    Span::raw(format!("{}  ", spinner())),
                    Span::styled(format!("Loading {}", page.pr.label()), dim),
                ])],
            };
            let height = lines.len() as u16;
            let top = area.y + area.height.saturating_sub(height) / 2;
            let indent = if matches!(page.detail, Load::Loading) {
                let text_width = page.pr.label().width() as u16 + 11;
                let indent = x + width.saturating_sub(text_width) / 2;
                // A moving field while it loads, the words on a plate over it.
                if let Some(ink) = ink {
                    let field =
                        Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2));
                    art::field(f.buffer_mut(), field, ink, Field::Ripple, art::clock());
                    let plate = Rect::new(
                        indent.saturating_sub(3),
                        top.saturating_sub(1),
                        text_width + 6,
                        3,
                    )
                    .intersection(area);
                    f.render_widget(Clear, plate);
                    art::surface(f.buffer_mut(), plate, ink);
                    art::shadow(f.buffer_mut(), plate, ink);
                }
                indent
            } else {
                x
            };
            f.render_widget(
                Paragraph::new(lines),
                Rect::new(indent, top, width, height.min(area.height)),
            );
            footer(f, page, area, x, width);
            return;
        }
    };

    // The header: the title, one quiet line about it, then the tabs.
    let mut header = vec![Line::default()];
    for line in wrap(&detail.title, w).into_iter().take(2) {
        header.push(Line::from(Span::styled(line, bold)));
    }
    let mut meta = vec![
        Span::styled(
            format!("● {}", detail.state.label()),
            Style::default().fg(state_color(detail.state)),
        ),
        Span::styled(
            format!(
                "   {} · {} · {}",
                detail.pr.label(),
                detail.author,
                pr::ago(&detail.updated, now)
            ),
            dim,
        ),
    ];
    if refreshing {
        meta.push(Span::styled(format!("   {}", spinner()), dim));
    }
    let label_x = x + format!("● {}   ", detail.state.label()).width() as u16;
    let label_width = (detail.pr.label().width() as u16).min((x + width).saturating_sub(label_x));
    page.number_hit = Some(Rect::new(
        label_x,
        area.y + header.len() as u16,
        label_width,
        1,
    ));
    let size = match detail.kind {
        Kind::Pull => vec![
            Span::styled(format!("+{}", detail.additions), Style::default().fg(GREEN)),
            Span::raw(" "),
            Span::styled(format!("−{}", detail.deletions), Style::default().fg(RED)),
        ],
        Kind::Issue => {
            let comments = detail
                .timeline
                .iter()
                .filter(|e| matches!(e.kind, EventKind::Comment { .. }))
                .count();
            match comments {
                0 => Vec::new(),
                1 => vec![Span::styled("1 comment", dim)],
                n => vec![Span::styled(format!("{n} comments"), dim)],
            }
        }
    };
    header.push(spread(meta, size, w));
    header.push(Line::default());
    let tabs_row = area.y + header.len() as u16;
    let mut tabs = Vec::new();
    page.tab_hits.clear();
    let mut at = x;
    let comment_count = detail
        .timeline
        .iter()
        .filter(|e| quoted(e).is_some())
        .count();
    let comments_label = match comment_count {
        0 => "Comments".to_owned(),
        n => format!("Comments {n}"),
    };
    let tab_labels = [
        ("Summary", TabHit::Summary, page.tab == Tab::Summary),
        ("Timeline", TabHit::Timeline, page.tab == Tab::Timeline),
        (
            comments_label.as_str(),
            TabHit::Comments,
            page.tab == Tab::Comments,
        ),
        ("Code", TabHit::Code, false),
        ("Checks", TabHit::Checks, page.tab == Tab::Checks),
    ];
    let shown_tabs = match page.kind {
        Kind::Pull => &tab_labels[..],
        Kind::Issue => &tab_labels[..3],
    };
    for &(label, hit, on) in shown_tabs {
        let text = if hit == TabHit::Code && page.code {
            format!("{} Code", spinner())
        } else {
            label.to_owned()
        };
        let width = text.width() as u16;
        page.tab_hits.push((Rect::new(at, tabs_row, width, 1), hit));
        at += width + 4;
        tabs.push(Span::styled(text, if on { bold } else { dim }));
        tabs.push(Span::raw("    "));
    }
    let checks = match pr::checks_status(&detail.checks) {
        Some(status) => {
            let (glyph, color) = check_glyph(status);
            vec![
                Span::styled(format!("{glyph} "), Style::default().fg(color)),
                Span::styled(pr::describe_checks(&detail.checks), dim),
            ]
        }
        None => Vec::new(),
    };
    header.push(spread(tabs, checks, w));
    header.push(Line::default());

    let header_height = header.len() as u16;
    page.header_area = Rect::new(x, area.y, width, header_height.min(area.height));
    let shown: Vec<Line> = header
        .iter()
        .enumerate()
        .map(
            |(index, line)| match page.selection.and_then(|s| s.columns(index)) {
                Some((from, to)) => highlight(line, from, to),
                None => line.clone(),
            },
        )
        .collect();
    f.render_widget(Paragraph::new(shown), page.header_area);
    page.header = header;

    // The body scrolls between the header and the footer.
    let footer_height = footer_height(page, width);
    let body_top = area.y + header_height;
    let body_height = (area.y + area.height)
        .saturating_sub(body_top + footer_height + 1)
        .max(1);
    page.body_area = Rect::new(x, body_top, width, body_height);
    let job = page.log.and_then(|job| logs.jobs.get(&job));
    if let (false, Some(Load::Ready(log))) = (page.log_init, job) {
        let (steps, groups, pick) = job_opening(log);
        page.open_steps = steps;
        // With `e` on, every group is open already; one in the set would close.
        page.open_groups = if page.expanded {
            HashSet::new()
        } else {
            groups
        };
        page.pick = pick;
        page.log_init = true;
        page.jump = true;
    }
    let stamp = match page.tab {
        Tab::Checks => {
            let mut hasher = DefaultHasher::new();
            (page.check, page.log, page.pick, logs.generation).hash(&mut hasher);
            let mut open: Vec<_> = page.open_steps.iter().collect();
            open.sort();
            open.hash(&mut hasher);
            let mut groups: Vec<_> = page.open_groups.iter().collect();
            groups.sort();
            groups.hash(&mut hasher);
            // Running checks count their seconds.
            if pr::any_running(&detail.checks) {
                now.hash(&mut hasher);
            }
            hasher.finish()
        }
        _ => 0,
    };
    let key = (width, page.tab, page.expanded, gallery.generation, stamp);
    if page.cache.as_ref().is_none_or(|(k, _)| *k != key) {
        page.check = page.check.min(detail.checks.len().saturating_sub(1));
        let body = match page.tab {
            Tab::Summary => summary(detail, w, gallery),
            Tab::Timeline => {
                let listed = timeline(detail, w, now, page.expanded, page.focus, gallery);
                page.starts = listed.starts;
                Body {
                    lines: listed.lines,
                    figures: listed.figures,
                }
            }
            Tab::Comments => {
                let listed = comments(detail, w, now, page.focus, page.expanded, gallery);
                page.starts = listed.starts;
                Body {
                    lines: listed.lines,
                    figures: listed.figures,
                }
            }
            Tab::Checks => match page.log {
                Some(_) => {
                    let view = job_view(page.picked_check(), job, page, w, now);
                    page.fold_rows = view.rows;
                    page.step_errors = view.errors;
                    Body {
                        lines: view.lines,
                        figures: Vec::new(),
                    }
                }
                None => {
                    let (lines, rows) = checks_tab(detail, page.check, w, now);
                    page.check_rows = rows;
                    Body {
                        lines,
                        figures: Vec::new(),
                    }
                }
            },
        };
        page.cache = Some((key, body));
    }
    if page.tab == Tab::Checks && page.log.is_some() {
        let rows = body_height as usize;
        let header = page
            .fold_rows
            .iter()
            .find(|(_, fold)| *fold == page.pick)
            .map(|(row, _)| *row);
        // A job opens on its failed step, scrolled to its last error with the output before it.
        if let Some(header) = header.filter(|_| page.jump) {
            page.jump = false;
            page.follow = false;
            page.scroll = header.saturating_sub(1);
            if let Some(&error) = page.step_errors.get(&page.pick.step())
                && error + 3 > page.scroll + rows
            {
                page.scroll = (error + 3).saturating_sub(rows);
            }
        }
        if let Some(header) = header.filter(|_| std::mem::take(&mut page.follow)) {
            page.scroll = page.scroll.min(header.saturating_sub(1));
            if header >= page.scroll + rows {
                page.scroll = header + 1 - rows;
            }
        }
    }
    // Keep the picked check, and its workflow's heading above it, in view.
    if std::mem::take(&mut page.follow)
        && page.tab == Tab::Checks
        && page.log.is_none()
        && let Some(&(row, _)) = page.check_rows.iter().find(|(_, i)| *i == page.check)
    {
        let rows = body_height as usize;
        page.scroll = page.scroll.min(row.saturating_sub(2));
        if row >= page.scroll + rows {
            page.scroll = row + 1 - rows;
        }
    }
    let Some((_, body)) = page.cache.as_ref() else {
        return;
    };
    // The picked comment comes into view a line below the top.
    if std::mem::take(&mut page.reveal)
        && let Some(start) = page.focus.and_then(|i| page.starts.get(i))
    {
        page.scroll = start.saturating_sub(1);
    }
    let last = body.lines.len().saturating_sub(body_height as usize);
    page.scroll = page.scroll.min(last);
    let visible: Vec<Line> = body
        .lines
        .iter()
        .enumerate()
        .skip(page.scroll)
        .take(body_height as usize)
        .map(|(index, line)| {
            match page
                .selection
                .and_then(|s| s.columns(page.header.len() + index))
            {
                Some((from, to)) => highlight(line, from, to),
                None => line.clone(),
            }
        })
        .collect();
    f.render_widget(Paragraph::new(visible), page.body_area);

    // Pictures go over the rows held for them; until they arrive, a quiet note does.
    for figure in &body.figures {
        let row = figure.top;
        let on_screen =
            row + (figure.rows as usize) > page.scroll && row < page.scroll + body_height as usize;
        if !on_screen {
            continue;
        }
        match gallery.shown(&figure.href) {
            Shown::Image | Shown::Video => gallery.place(figure, page.body_area, page.scroll),
            state => {
                let note = if state == Shown::Loading {
                    format!("{}  loading", spinner())
                } else {
                    "▣  couldn't show this · click to open it".to_owned()
                };
                let y = row.max(page.scroll) - page.scroll;
                let note_x = x + figure.x + figure.cols.saturating_sub(note.width() as u16) / 2;
                f.render_widget(
                    Paragraph::new(Span::styled(note.clone(), dim)),
                    Rect::new(
                        note_x,
                        body_top + y as u16,
                        (note.width() as u16).min(width),
                        1,
                    ),
                );
            }
        }
    }

    if let Some(ink) = ink {
        art::chrome(
            f.buffer_mut(),
            area,
            x,
            width,
            header_height.saturating_sub(1),
            ink,
        );
    }
    footer(f, page, area, x, width);
    if let Mode::Help = page.mode {
        draw_help(f, area, page.kind, ink);
    }
}

fn footer_height(page: &Page, width: u16) -> u16 {
    match &page.mode {
        Mode::Compose { text, .. } => {
            let body: String = text.iter().collect();
            (wrap(&body, width as usize).len().max(1) as u16 + 2).min(10)
        }
        _ => 1,
    }
}

fn footer(f: &mut Frame, page: &Page, area: Rect, x: u16, width: u16) {
    let dim = Style::default().fg(DIM);
    let accent = Style::default().add_modifier(Modifier::BOLD);
    let y = area.y + area.height.saturating_sub(1);
    let methods = match &page.detail {
        Load::Ready(detail) => detail.methods.clone(),
        _ => Vec::new(),
    };
    let base = match &page.detail {
        Load::Ready(detail) => detail.base.clone(),
        _ => String::new(),
    };
    let line = match &page.mode {
        Mode::Compose {
            text,
            cursor,
            reply,
            refused,
        } => {
            let height = footer_height(page, width);
            let top = y + 1 - height;
            let title = match reply {
                Some(who) => format!("Reply to {who} on #{}", page.pr.number),
                None => format!("Comment on #{}", page.pr.number),
            };
            // Why enter didn't send, until the next key; else the keys.
            let hint = match refused {
                Some(why) => Span::styled(format!("   {why}"), Style::default().fg(RED)),
                None => Span::styled("   enter send · alt+enter new line · esc cancel", dim),
            };
            let mut lines = vec![
                Line::default(),
                Line::from(vec![Span::styled(title, accent), hint]),
            ];
            let mut body: String = text[..*cursor].iter().collect();
            body.push('▏');
            body.extend(&text[*cursor..]);
            for line in wrap(&body, width as usize) {
                lines.push(Line::from(line));
            }
            let skip = lines.len().saturating_sub(height as usize);
            let rect = Rect::new(x, top.saturating_sub(1), width, height + 1);
            f.render_widget(Clear, rect);
            f.render_widget(
                Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()),
                rect,
            );
            return;
        }
        Mode::Rerun(choices) => {
            let mut spans = Vec::new();
            for (key, action) in choices {
                if let Action::Rerun { scope, .. } = action {
                    spans.push(Span::styled(format!("{key} "), accent));
                    spans.push(Span::styled(format!("{}   ", scope.label()), dim));
                }
            }
            spans.push(Span::styled("esc cancel", dim));
            Line::from(spans)
        }
        Mode::Merge => {
            let mut spans = vec![Span::styled(format!("Merge into {base}   "), accent)];
            for method in &methods {
                spans.push(Span::styled(format!("{} ", method.key()), accent));
                spans.push(Span::styled(format!("{}   ", method.label()), dim));
            }
            spans.push(Span::styled("esc cancel", dim));
            Line::from(spans)
        }
        Mode::Close => {
            let mut spans = vec![Span::styled(
                format!("Close #{} as   ", page.pr.number),
                accent,
            )];
            for reason in CloseReason::ALL {
                spans.push(Span::styled(format!("{} ", reason.key()), accent));
                spans.push(Span::styled(format!("{}   ", reason.label()), dim));
            }
            spans.push(Span::styled("esc cancel", dim));
            Line::from(spans)
        }
        Mode::Confirm(action) => {
            let question = match action {
                Action::Merge(method) => {
                    format!("{} #{} into {base}?", method.label(), page.pr.number)
                }
                Action::Close => format!("Close #{}?", page.pr.number),
                Action::CloseAs(reason) => {
                    format!("Close #{} as {}?", page.pr.number, reason.label())
                }
                Action::Reopen => format!("Reopen #{}?", page.pr.number),
                Action::Draft => format!("Convert #{} to a draft?", page.pr.number),
                Action::Approve => format!("Approve #{}?", page.pr.number),
                Action::Ready => format!("Mark #{} ready for review?", page.pr.number),
                Action::Comment(_) => "Post the comment?".to_owned(),
                Action::Rerun { scope, name, .. } => match scope {
                    pr::Rerun::Failed => format!("Re-run failed jobs in {name}?"),
                    pr::Rerun::Job(_) => format!("Re-run {name}?"),
                    pr::Rerun::All => format!("Re-run all jobs in {name}?"),
                },
            };
            Line::from(vec![
                Span::styled(question, accent),
                Span::styled("   enter confirm · esc cancel", dim),
            ])
        }
        Mode::Normal | Mode::Help => match (&page.busy, &page.toast) {
            (Some(busy), _) => Line::from(vec![
                Span::raw(format!("{} ", spinner())),
                Span::styled(busy.clone(), accent),
            ]),
            (None, Some(toast)) if !toast.is_empty() => Line::from(toast.clone()),
            _ if page.code => Line::from(Span::styled(
                format!("{} Loading the diff…", spinner()),
                dim,
            )),
            _ if page.tab == Tab::Comments => keys_line(
                &[
                    ("j/k", "pick"),
                    ("enter", "reply"),
                    ("c", "comment"),
                    ("←→", "tabs"),
                    ("q", "back"),
                ],
                width as usize,
            ),
            _ if page.tab == Tab::Checks && page.log.is_some() => {
                let mut keys = LOG_KEYS;
                if page.expanded {
                    keys[1].1 = "close all groups";
                }
                keys_line(&keys, width as usize)
            }
            _ if page.tab == Tab::Checks => keys_line(&CHECK_KEYS, width as usize),
            _ if page.focus.is_some() => keys_line(
                &[("enter", "reply"), ("[ ]", "pick"), ("esc", "done")],
                width as usize,
            ),
            _ => match page.kind {
                Kind::Pull => keys_line(&KEYS, width as usize),
                Kind::Issue => keys_line(&ISSUE_KEYS, width as usize),
            },
        },
    };
    f.render_widget(Paragraph::new(line), Rect::new(x, y, width, 1));
}

fn draw_help(f: &mut Frame, area: Rect, kind: Kind, ink: Option<art::Ink>) {
    let keys: &[(&str, &str)] = match kind {
        Kind::Pull => &[
            ("←→, tab", "Summary, Timeline, Comments, Code, Checks"),
            ("1, 2, 3, 4, 5", "the same tabs"),
            ("↑↓, enter", "Checks: a job's steps and log"),
            ("j/k, ctrl+d/u", "scroll a line, half a screen"),
            ("space/b, g/G", "a screen, top and bottom"),
            ("drag", "select text; it's copied"),
            ("click", "a picture opens it, a video plays"),
            ("e", "long comments and quotes, or a job's groups"),
            ("m", "merge: squash, merge commit or rebase"),
            ("x", "close, or reopen a closed one"),
            ("r", "ready or draft; on Checks, re-run"),
            ("a", "approve"),
            ("c", "comment"),
            ("] [, enter", "pick a comment, reply quoting it"),
            ("j/k on Comments", "pick the next or previous comment"),
            ("o", "open on GitHub, or the picked check"),
            ("y, Y, n", "copy the checkout command, link, number"),
            ("click #", "the number in the header copies it"),
            ("R", "reload"),
            ("q, esc", "back to the list, or quit"),
        ],
        Kind::Issue => &[
            ("←→, tab", "Summary, Timeline, Comments"),
            ("1, 2, 3", "the same tabs"),
            ("j/k, ctrl+d/u", "scroll a line, half a screen"),
            ("space/b, g/G", "a screen, top and bottom"),
            ("drag", "select text; it's copied"),
            ("click", "a picture opens it, a video plays"),
            ("e", "expand or cut short long comments and quotes"),
            ("x", "close as done or not planned, reopen"),
            ("c", "comment"),
            ("] [, enter", "pick a comment, reply quoting it"),
            ("j/k on Comments", "pick the next or previous comment"),
            ("o", "open on GitHub"),
            ("y, n", "copy the link, number"),
            ("click #", "the number in the header copies it"),
            ("R", "reload"),
            ("q, esc", "back to the list, or quit"),
        ],
    };
    let width = 54.min(area.width.saturating_sub(4));
    let height = (keys.len() as u16 + 2).min(area.height);
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    let lines: Vec<Line> = keys
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(
                    format!(" {key:<15}"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled((*what).to_owned(), Style::default().fg(DIM)),
            ])
        })
        .collect();
    f.render_widget(Clear, rect);
    if let Some(ink) = ink {
        art::surface(f.buffer_mut(), rect, ink);
        art::shadow(f.buffer_mut(), rect, ink);
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Keys ")),
        rect,
    );
}

/// Word-wraps `text` to `width` columns, keeping its own line breaks.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let w = word.width();
            if used > 0 && used + 1 + w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            // A word wider than the line breaks wherever it must.
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > width && used > 0 {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        out.push(line);
    }
    out
}

/// A label in the summary's left column, then its value.
fn field(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{label:<LABEL$}"),
        Style::default().fg(DIM),
    )];
    spans.extend(value);
    Line::from(spans)
}

/// Status, then only what has something to say: reviewers, labels, checks
/// that need you, then the description with its pictures.
fn summary(detail: &Detail, width: usize, gallery: &mut Gallery) -> Body {
    let dim = Style::default().fg(DIM);
    let mut lines = Vec::new();

    if detail.kind == Kind::Issue {
        let (status, color) = match detail.state {
            State::Completed => ("Closed as completed", PURPLE),
            State::NotPlanned => ("Closed as not planned", DIM),
            _ => ("Open", GREEN),
        };
        lines.push(field(
            "Status",
            vec![Span::styled(status, Style::default().fg(color))],
        ));
        lines.push(field(
            "Assignees",
            vec![if detail.assignees.is_empty() {
                Span::styled("No one", dim)
            } else {
                Span::raw(detail.assignees.join("  "))
            }],
        ));
        if !detail.labels.is_empty() {
            lines.push(field(
                "Labels",
                vec![Span::styled(detail.labels.join("  "), dim)],
            ));
        }
        return with_description(lines, detail, width, gallery);
    }

    let (status, color) = match (detail.state, detail.mergeability) {
        (State::Merged, _) => ("Merged".to_owned(), PURPLE),
        (State::Closed | State::Completed | State::NotPlanned, _) => {
            ("Closed without merging".to_owned(), RED)
        }
        (State::Draft, _) => ("Draft".to_owned(), DIM),
        (_, Mergeability::Ready) => ("Ready to merge".to_owned(), GREEN),
        (_, Mergeability::Conflicts) => (format!("Conflicts with {}", detail.base), RED),
        (_, Mergeability::Blocked) => ("Blocked by required reviews or checks".to_owned(), YELLOW),
        (_, Mergeability::Behind) => (format!("Behind {}", detail.base), YELLOW),
        (_, Mergeability::Unstable) => ("Mergeable, some checks fail".to_owned(), YELLOW),
        (_, Mergeability::Unknown) => ("Checking mergeability…".to_owned(), DIM),
    };
    lines.push(field(
        "Status",
        vec![Span::styled(status, Style::default().fg(color))],
    ));
    lines.push(field(
        "Branch",
        vec![
            Span::raw(detail.head.clone()),
            Span::styled(format!(" → {}", detail.base), dim),
        ],
    ));
    if !detail.reviewers.is_empty() {
        let reviewers = detail
            .reviewers
            .iter()
            .flat_map(|r| {
                let (glyph, color) = verdict_glyph(r.verdict);
                [
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::raw(format!("{}   ", r.login)),
                ]
            })
            .collect();
        lines.push(field("Reviewers", reviewers));
    }
    if !detail.labels.is_empty() {
        lines.push(field(
            "Labels",
            vec![Span::styled(detail.labels.join("  "), dim)],
        ));
    }

    // Passing checks are summed up beside the tabs; only those that need you show here.
    let mut checks: Vec<_> = detail
        .checks
        .iter()
        .filter(|c| matches!(c.status, CheckStatus::Failed | CheckStatus::Running))
        .collect();
    if !checks.is_empty() {
        checks.sort_by_key(|c| c.status != CheckStatus::Failed);
        lines.push(Line::default());
        for check in checks {
            let (glyph, color) = check_glyph(check.status);
            lines.push(field(
                "",
                vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::raw(clip(&check.label(), width.saturating_sub(LABEL + 2))),
                ],
            ));
        }
    }

    with_description(lines, detail, width, gallery)
}

/// The summary's fields, then two blank lines and the description.
fn with_description(
    mut lines: Vec<Line<'static>>,
    detail: &Detail,
    width: usize,
    gallery: &mut Gallery,
) -> Body {
    lines.push(Line::default());
    lines.push(Line::default());
    let offset = lines.len();
    gallery.sign(&detail.media);
    let description = description(&detail.body, width as u16, gallery);
    lines.extend(description.lines);
    Body {
        lines,
        figures: description
            .figures
            .into_iter()
            .map(|mut figure| {
                figure.top += offset;
                figure
            })
            .collect(),
    }
}

/// How bad a check is, to list the worst first.
fn rank(status: CheckStatus) -> u8 {
    match status {
        CheckStatus::Failed => 0,
        CheckStatus::Running => 1,
        CheckStatus::Passed => 2,
        CheckStatus::Skipped => 3,
    }
}

/// The checks grouped by workflow, each group's worst first, and the worst
/// groups first, with status contexts last: as indices into `checks`.
fn check_groups(checks: &[pr::Check]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, check) in checks.iter().enumerate() {
        match groups.iter_mut().find(|(name, _)| *name == check.workflow) {
            Some((_, members)) => members.push(index),
            None => groups.push((check.workflow.clone(), vec![index])),
        }
    }
    for (_, members) in &mut groups {
        members.sort_by_key(|&i| (rank(checks[i].status), checks[i].name.to_lowercase()));
    }
    groups.sort_by_key(|(name, members)| {
        (
            name.is_empty(),
            members.first().map_or(u8::MAX, |&i| rank(checks[i].status)),
            name.to_lowercase(),
        )
    });
    groups
}

/// Every check in the order the Checks tab lists them.
fn check_order(checks: &[pr::Check]) -> Vec<usize> {
    check_groups(checks)
        .into_iter()
        .flat_map(|(_, members)| members)
        .collect()
}

/// The Checks tab: each workflow's checks under its name, with how long each
/// took, and the body rows each check is on.
fn checks_tab(
    detail: &Detail,
    picked: usize,
    width: usize,
    now: i64,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if detail.checks.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "No checks ran on this pull request.",
                dim,
            ))],
            Vec::new(),
        );
    }
    let mut lines = Vec::new();
    let mut rows = Vec::new();
    let mut index = 0;
    for (workflow, members) in check_groups(&detail.checks) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        let title = if workflow.is_empty() {
            "Other checks".to_owned()
        } else {
            workflow
        };
        lines.push(Line::from(vec![
            Span::styled(
                clip(&title.to_uppercase(), width),
                dim.add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {}", members.len()), dim),
        ]));
        lines.push(Line::default());
        for member in members {
            let check = &detail.checks[member];
            let on = index == picked;
            let (glyph, color) = check_glyph(check.status);
            let took = check.elapsed(now).map(pr::duration).unwrap_or_default();
            rows.push((lines.len(), index));
            lines.push(spread(
                vec![
                    Span::styled(if on { "▌ " } else { "  " }, bold),
                    Span::styled(format!("{glyph}  "), Style::default().fg(color)),
                    Span::styled(check.name.clone(), if on { bold } else { Style::default() }),
                ],
                vec![Span::styled(took, dim)],
                width,
            ));
            index += 1;
        }
    }
    (lines, rows)
}

/// What a job opens with as it arrives: its failed steps, or the running one
/// while it runs; the groups opened; and the step picked.
fn job_opening(log: &pr::JobLog) -> (HashSet<usize>, HashSet<(usize, usize)>, Fold) {
    let mut steps: HashSet<usize> = (0..log.steps.len())
        .filter(|&i| log.steps[i].status == CheckStatus::Failed)
        .collect();
    if steps.is_empty()
        && let Some(running) = log
            .steps
            .iter()
            .position(|s| s.status == CheckStatus::Running && !s.queued)
    {
        steps.insert(running);
    }
    // A failed step's last error is what it opens on, so a group holding it opens too.
    let mut groups = HashSet::new();
    for &index in &steps {
        let last =
            log.steps[index]
                .items
                .iter()
                .enumerate()
                .rev()
                .find_map(|(at, item)| match item {
                    pr::LogItem::Line(line) => (line.kind == pr::LineKind::Error).then_some(None),
                    pr::LogItem::Group { lines, .. } => lines
                        .iter()
                        .any(|l| l.kind == pr::LineKind::Error)
                        .then_some(Some(at)),
                });
        if let Some(Some(at)) = last {
            groups.insert((index, at));
        }
    }
    let pick = Fold::Step(steps.iter().copied().min().unwrap_or(0));
    (steps, groups, pick)
}

/// A job's steps and log as the Checks tab lays them out.
struct JobView {
    lines: Vec<Line<'static>>,
    /// The line each step's and shown group's header is on.
    rows: Vec<(usize, Fold)>,
    /// Each step's last error line.
    errors: HashMap<usize, usize>,
}

/// An Actions job like GitHub shows it: its steps, each opened to its log
/// with line numbers, groups folded until `e`, and the log's own colors.
fn job_view(
    check: Option<&pr::Check>,
    job: Option<&Load<pr::JobLog>>,
    page: &Page,
    width: usize,
    now: i64,
) -> JobView {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut view = JobView {
        lines: Vec::new(),
        rows: Vec::new(),
        errors: HashMap::new(),
    };
    let lines = &mut view.lines;
    if let Some(check) = check {
        let (glyph, color) = check_glyph(check.status);
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph}  "), Style::default().fg(color)),
            Span::styled(clip(&check.name, width.saturating_sub(3)), bold),
        ]));
        let took = check.elapsed(now).map(pr::duration);
        let state = match (check.status, took) {
            (CheckStatus::Failed, Some(t)) => format!("failed after {t}"),
            (CheckStatus::Passed, Some(t)) => format!("succeeded in {t}"),
            (CheckStatus::Running, Some(t)) => format!("running for {t}"),
            (CheckStatus::Failed, None) => "failed".to_owned(),
            (CheckStatus::Passed, None) => "succeeded".to_owned(),
            (CheckStatus::Running, None) => "waiting".to_owned(),
            (CheckStatus::Skipped, _) => "skipped".to_owned(),
        };
        let about = if check.workflow.is_empty() {
            state
        } else {
            format!("{} · {state}", check.workflow)
        };
        lines.push(Line::from(Span::styled(
            format!("   {}", clip(&about, width.saturating_sub(3))),
            dim,
        )));
        lines.push(Line::default());
    }
    let log = match job {
        None | Some(Load::Loading) => {
            lines.push(Line::from(vec![
                Span::raw(format!("{}  ", spinner())),
                Span::styled("Loading the job's steps", dim),
            ]));
            return view;
        }
        Some(Load::Failed(e)) => {
            lines.extend(wrap(e, width).into_iter().map(Line::from));
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "R tries again, o opens it on GitHub",
                dim,
            )));
            return view;
        }
        Some(Load::Ready(log)) => log,
    };
    if log.steps.is_empty() {
        let note = match check.map(|c| c.status) {
            Some(CheckStatus::Skipped) => "This job was skipped, so it has no steps.",
            _ if !log.done => "Waiting for a runner.",
            _ => "This job has no steps.",
        };
        lines.push(Line::from(Span::styled(note, dim)));
        return view;
    }
    for (index, step) in log.steps.iter().enumerate() {
        let picked = page.pick == Fold::Step(index);
        let open = page.open_steps.contains(&index);
        let (glyph, color) = if step.queued {
            ("○", DIM)
        } else {
            check_glyph(step.status)
        };
        let took = match step.status {
            CheckStatus::Skipped => String::new(),
            _ => step.elapsed(now).map(pr::duration).unwrap_or_default(),
        };
        if open && index > 0 {
            lines.push(Line::default());
        }
        view.rows.push((lines.len(), Fold::Step(index)));
        lines.push(spread(
            vec![
                Span::styled(if picked { "▌ " } else { "  " }, bold),
                Span::styled(if open { "▾ " } else { "▸ " }, dim),
                Span::styled(format!("{glyph}  "), Style::default().fg(color)),
                Span::styled(
                    step.name.clone(),
                    if picked {
                        bold
                    } else if step.status == CheckStatus::Skipped || step.queued {
                        dim
                    } else {
                        Style::default()
                    },
                ),
            ],
            vec![Span::styled(took, dim)],
            width,
        ));
        if !open {
            continue;
        }
        if step.items.is_empty() {
            let note = if !log.done {
                "The log appears here once the job finishes. o follows it live on GitHub."
            } else if let Some(missing) = &log.missing {
                missing.as_str()
            } else {
                "No output."
            };
            for line in wrap(note, width.saturating_sub(6)) {
                lines.push(Line::from(Span::styled(format!("      {line}"), dim)));
            }
            lines.push(Line::default());
            continue;
        }
        let digits = (step.earlier + step.lines()).max(1).to_string().len();
        let mut number = step.earlier;
        if step.earlier > 0 {
            lines.push(Line::from(Span::styled(
                format!(
                    "      … {} earlier lines · o opens the full log",
                    step.earlier
                ),
                dim,
            )));
        }
        let push = |lines: &mut Vec<Line<'static>>,
                    number: usize,
                    indent: usize,
                    picked: bool,
                    spans: Vec<Span<'static>>| {
            let gutter = 4 + digits + 2 + indent;
            let rows = wrap_spans(
                spans,
                width.saturating_sub(gutter).max(1),
                width.saturating_sub(gutter + 2).max(1),
            );
            for (row, part) in rows.into_iter().enumerate() {
                let gutter = if row == 0 && picked {
                    let gutter = format!("  {number:>digits$}  {}", " ".repeat(indent));
                    lines.push(Line::from(
                        [Span::styled("▌ ", bold), Span::styled(gutter, dim)]
                            .into_iter()
                            .chain(part)
                            .collect::<Vec<_>>(),
                    ));
                    continue;
                } else if row == 0 {
                    format!("    {number:>digits$}  {}", " ".repeat(indent))
                } else {
                    // Wrapped rows hang under the text, past the number.
                    " ".repeat(4 + digits + 2 + indent + 2)
                };
                let mut spans = vec![Span::styled(gutter, dim)];
                spans.extend(part);
                lines.push(Line::from(spans));
            }
        };
        for (at, item) in step.items.iter().enumerate() {
            match item {
                pr::LogItem::Line(line) => {
                    number += 1;
                    if line.kind == pr::LineKind::Error {
                        view.errors.insert(index, lines.len());
                    }
                    push(lines, number, 0, false, log_spans(line));
                }
                pr::LogItem::Group {
                    title,
                    lines: group,
                } => {
                    number += 1;
                    let fold = Fold::Group(index, at);
                    // `e` turns every group the other way.
                    let shown = page.expanded != page.open_groups.contains(&(index, at));
                    let picked = page.pick == fold;
                    let mut spans = ansi_spans(title, if picked { bold } else { Style::default() });
                    if shown {
                        spans.insert(0, Span::styled("▾ ", dim));
                    } else {
                        spans.insert(0, Span::styled("▸ ", dim));
                        if !group.is_empty() {
                            spans.push(Span::styled(format!(" · {} lines", group.len()), dim));
                        }
                    }
                    view.rows.push((lines.len(), fold));
                    push(lines, number, 0, picked, spans);
                    for line in group {
                        number += 1;
                        if shown {
                            if line.kind == pr::LineKind::Error {
                                view.errors.insert(index, lines.len());
                            }
                            push(lines, number, 2, false, log_spans(line));
                        }
                    }
                }
            }
        }
        lines.push(Line::default());
    }
    if !log.done {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Still running: the steps update every few seconds.",
            dim,
        )));
    }
    view
}

/// A log line in its kind's look, with its own colors.
fn log_spans(line: &pr::LogLine) -> Vec<Span<'static>> {
    let (label, style) = match line.kind {
        pr::LineKind::Error => (Some("Error: "), Style::default().fg(RED)),
        pr::LineKind::Warning => (Some("Warning: "), Style::default().fg(YELLOW)),
        pr::LineKind::Notice => (Some("Notice: "), Style::default()),
        pr::LineKind::Debug => (None, Style::default().fg(DIM)),
        pr::LineKind::Command => (None, Style::default().fg(BLUE)),
        pr::LineKind::Plain => (None, Style::default()),
    };
    let mut spans = Vec::new();
    if let Some(label) = label {
        spans.push(Span::styled(label, style.add_modifier(Modifier::BOLD)));
    }
    spans.extend(ansi_spans(&line.text, style));
    spans
}

/// `text` as spans styled by its color escapes, over `base`; other escapes go.
fn ansi_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut style = base;
    let mut run = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                let mut params = String::new();
                let mut end = None;
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        end = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if end == Some('m') {
                    if !run.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut run), style));
                    }
                    style = sgr(&params, style, base);
                }
            }
            continue;
        }
        match c {
            '\t' => run.push_str("    "),
            c if c.is_control() => {}
            c => run.push(c),
        }
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style));
    }
    spans
}

/// `style` after one color escape's parameters; backgrounds are left out,
/// since they fight the page's.
fn sgr(params: &str, mut style: Style, base: Style) -> Style {
    let codes: Vec<u16> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = base,
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            code @ 30..=37 => style.fg = Some(Color::Indexed((code - 30) as u8)),
            code @ 90..=97 => style.fg = Some(Color::Indexed((code - 90 + 8) as u8)),
            39 => style.fg = base.fg,
            code @ (38 | 48) => {
                let color = match codes.get(i + 1) {
                    Some(5) => {
                        let color = codes.get(i + 2).map(|&n| Color::Indexed(n as u8));
                        i += 2;
                        color
                    }
                    Some(2) => {
                        let channel = |at: usize| codes.get(at).map(|&n| n as u8);
                        let color = channel(i + 2)
                            .zip(channel(i + 3))
                            .zip(channel(i + 4))
                            .map(|((r, g), b)| Color::Rgb(r, g, b));
                        i += 4;
                        color
                    }
                    _ => None,
                };
                if code == 38 && color.is_some() {
                    style.fg = color;
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// Spans wrapped to `first` columns, then `rest` for the rows after, at
/// spaces where a row has one, else anywhere.
fn wrap_spans(spans: Vec<Span<'static>>, first: usize, rest: usize) -> Vec<Vec<Span<'static>>> {
    let mut width = first.max(1);
    let chars: Vec<char> = spans.iter().flat_map(|s| s.content.chars()).collect();
    let cell = |c: char| c.width().unwrap_or(0);
    let mut breaks = HashSet::new();
    let (mut start, mut used, mut space) = (0, 0, None);
    let mut i = 0;
    while i < chars.len() {
        let w = cell(chars[i]);
        if used + w > width && i > start {
            let at = space.filter(|&s| s > start).map_or(i, |s| s + 1);
            breaks.insert(at);
            width = rest.max(1);
            start = at;
            space = None;
            used = chars[start..i].iter().map(|&c| cell(c)).sum();
            continue;
        }
        if chars[i] == ' ' {
            space = Some(i);
        }
        used += w;
        i += 1;
    }
    let (mut rows, mut row) = (Vec::new(), Vec::new());
    let mut index = 0;
    for span in spans {
        let mut run = String::new();
        for c in span.content.chars() {
            if breaks.contains(&index) {
                if !run.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut run), span.style));
                }
                rows.push(std::mem::take(&mut row));
            }
            run.push(c);
            index += 1;
        }
        if !run.is_empty() {
            row.push(Span::styled(run, span.style));
        }
    }
    rows.push(row);
    rows
}

/// The description laid out by the reader's markdown, with room held for its
/// pictures when the terminal can draw them, and placeholders when it can't.
fn description(body: &str, width: u16, gallery: &mut Gallery) -> Body {
    let body = pr::visible(body);
    if body.is_empty() {
        return Body {
            lines: vec![Line::from(Span::styled(
                "No description.",
                Style::default().fg(DIM),
            ))],
            figures: Vec::new(),
        };
    }
    if !gallery.shows_media() {
        return Body {
            lines: markdown(&body, width, 0),
            figures: Vec::new(),
        };
    }
    with_media(&body, width, 0, gallery)
}

/// Markdown laid out by the reader, as lines `indent` columns in, with room
/// held for its pictures and videos and a label under each video. Only for a
/// terminal that can draw them.
fn with_media(body: &str, width: u16, indent: u16, gallery: &mut Gallery) -> Body {
    let prepared = prmedia::prepare(body);
    gallery.learn(&prepared);
    let mut laid = {
        let gallery = &*gallery;
        render::layout(&prepared.markdown, width.max(10), gallery.cell(), &|href| {
            gallery.size(href)
        })
    };
    for figure in &mut laid.figures {
        gallery.want(&figure.href);
        figure.x += indent;
    }
    let mut lines = rows(&laid.rows, indent);
    // Trailing blank rows go, but not the rows a picture at the end is drawn over.
    let held = laid
        .figures
        .iter()
        .map(|f| f.top + f.rows as usize + 1)
        .max()
        .unwrap_or(0);
    while lines.len() > held
        && lines
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        lines.pop();
    }
    for figure in &laid.figures {
        if !gallery.is_video(&figure.href) {
            continue;
        }
        let label = if gallery.is_buffering(&figure.href) {
            "◌  loading video · click or esc to stop"
        } else if gallery.is_playing(&figure.href) {
            "■  playing · click or esc to stop"
        } else {
            "▶  video · click to play"
        };
        let pad = figure.x + figure.cols.saturating_sub(label.width() as u16) / 2;
        let line = Line::from(vec![
            Span::raw(" ".repeat(pad as usize)),
            Span::styled(label, Style::default().fg(DIM)),
        ]);
        let at = figure.top + figure.rows as usize;
        if at < lines.len() {
            lines[at] = line;
        } else {
            lines.push(line);
        }
    }
    Body {
        lines,
        figures: laid.figures,
    }
}

/// The reader's laid-out rows as lines, `indent` columns in.
fn rows(rows: &[render::Row], indent: u16) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| {
            let mut spans = vec![Span::raw(" ".repeat(indent as usize))];
            let mut col = 0u16;
            for seg in &row.segs {
                if seg.x > col {
                    spans.push(Span::raw(" ".repeat((seg.x - col) as usize)));
                    col = seg.x;
                }
                col += seg.text.width() as u16;
                spans.push(Span::styled(seg.text.clone(), seg.style));
            }
            Line::from(spans)
        })
        .collect()
}

/// A comment quoted to start a reply, or `None` for an event with nothing to quote.
/// Why `body` can't be posted: nothing written, or a reply that's only the
/// comment it quotes.
fn empty_comment(body: &str, reply: bool) -> Option<&'static str> {
    let own = body
        .lines()
        .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('>'));
    if body.trim().is_empty() {
        Some("Can't post an empty comment; write something, or esc to cancel")
    } else if reply && !own {
        Some("Write your reply below the quote, or esc to cancel")
    } else {
        None
    }
}

fn quoted(event: &pr::Event) -> Option<String> {
    let body = match &event.kind {
        EventKind::Comment { body }
        | EventKind::Review { body, .. }
        | EventKind::Inline { body, .. } => pr::visible(body),
        _ => return None,
    };
    if body.is_empty() {
        return None;
    }
    // Only what the comment says itself: quoting its quotes too would nest
    // the whole thread one level deeper with every reply.
    let own: Vec<&str> = quote_lines(&body)
        .into_iter()
        .filter(|(quote, _)| !quote)
        .map(|(_, line)| line)
        .collect();
    let own = own.join("\n");
    let own = own.trim_matches('\n');
    let text = if own.trim().is_empty() {
        body.as_str()
    } else {
        own
    };
    let mut quote: String = text.lines().map(|line| format!("> {line}\n")).collect();
    quote.push('\n');
    Some(quote)
}

/// Each of `body`'s lines, and whether it quotes something: a `>` line
/// outside a code block.
fn quote_lines(body: &str) -> Vec<(bool, &str)> {
    let mut fenced = false;
    body.lines()
        .map(|line| {
            let start = line.trim_start();
            if start.starts_with("```") || start.starts_with("~~~") {
                fenced = !fenced;
            }
            (!fenced && start.starts_with('>'), line)
        })
        .collect()
}

/// `body` with each quote in it cut down to one line saying how long it was.
fn without_quotes(body: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut run = 0usize;
    let end_run = |out: &mut Vec<String>, run: &mut usize| {
        if *run > 0 {
            let lines = if *run == 1 {
                "1 line".to_owned()
            } else {
                format!("{run} lines")
            };
            out.extend([
                String::new(),
                format!("_▸ quote · {lines} · e shows it_"),
                String::new(),
            ]);
            *run = 0;
        }
    };
    for (quote, line) in quote_lines(body) {
        if quote {
            run += 1;
        } else {
            end_run(&mut out, &mut run);
            out.push(line.to_owned());
        }
    }
    end_run(&mut out, &mut run);
    out.join("\n")
}

/// A list of events laid out: its lines, the first line of each event (by
/// timeline index), and the pictures drawn over the lines.
struct Listed {
    lines: Vec<Line<'static>>,
    starts: Vec<usize>,
    figures: Vec<Figure>,
}

/// Which events a list of them holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Events {
    All,
    /// Only those with a comment to quote, each in full.
    Comments,
}

/// The timeline's lines, and the first line of each event. The `focus`
/// event, picked to reply to, has its rail drawn bright instead of dim.
fn timeline(
    detail: &Detail,
    width: usize,
    now: i64,
    expanded: bool,
    focus: Option<usize>,
    gallery: &mut Gallery,
) -> Listed {
    events(detail, width, now, expanded, focus, Events::All, gallery)
}

/// The Comments tab: every comment and review with something to say, in
/// full. `starts` is still by timeline index, as `focus` is.
fn comments(
    detail: &Detail,
    width: usize,
    now: i64,
    focus: Option<usize>,
    quotes: bool,
    gallery: &mut Gallery,
) -> Listed {
    let listed = events(detail, width, now, quotes, focus, Events::Comments, gallery);
    if !listed.lines.is_empty() {
        return listed;
    }
    let subject = match detail.kind {
        Kind::Pull => "pull request",
        Kind::Issue => "issue",
    };
    let empty = vec![Line::from(Span::styled(
        format!("No comments on this {subject} yet. c writes the first one."),
        Style::default().fg(DIM),
    ))];
    Listed {
        lines: empty,
        starts: listed.starts,
        figures: Vec::new(),
    }
}

fn events(
    detail: &Detail,
    width: usize,
    now: i64,
    expanded: bool,
    focus: Option<usize>,
    which: Events,
    gallery: &mut Gallery,
) -> Listed {
    // The Comments tab shows each comment whole; `e` shows quotes too.
    let (quotes, expanded) = (expanded, expanded || which == Events::Comments);
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    let mut starts = Vec::new();
    let mut figures = Vec::new();
    let indent = 4u16;
    let subject = match detail.kind {
        Kind::Pull => "pull request",
        Kind::Issue => "issue",
    };
    let closed = match detail.state {
        State::Completed => "Closed as completed",
        State::NotPlanned => "Closed as not planned",
        _ => "Closed",
    };
    let closed_color = state_color(detail.state);
    for (index, event) in detail.timeline.iter().enumerate() {
        starts.push(lines.len());
        if which == Events::Comments && quoted(event).is_none() {
            continue;
        }
        let picked = focus == Some(index);
        let accent = Style::default().fg(BLUE).add_modifier(Modifier::BOLD);
        let rail = if picked { accent } else { dim };
        let when = Span::styled(pr::ago(&event.at, now), dim);
        let actor = Span::styled(event.actor.clone(), bold);
        let (glyph, color, what, body): (&str, Color, Vec<Span<'static>>, Option<String>) =
            match &event.kind {
                EventKind::Opened => (
                    "●",
                    GREEN,
                    vec![actor, Span::styled(format!(" opened this {subject}"), dim)],
                    None,
                ),
                EventKind::Commit { sha, headline } => (
                    "○",
                    DIM,
                    vec![
                        Span::styled(format!("{sha} "), Style::default().fg(YELLOW)),
                        Span::raw(headline.clone()),
                        Span::styled(format!(" · {}", event.actor), dim),
                    ],
                    None,
                ),
                EventKind::Comment { body } => (
                    "◆",
                    DIM,
                    vec![actor, Span::styled(" commented", dim)],
                    Some(body.clone()),
                ),
                EventKind::Review { verdict, body } => {
                    let (glyph, color) = verdict_glyph(*verdict);
                    (
                        glyph,
                        color,
                        vec![actor, Span::styled(format!(" {}", verdict.label()), dim)],
                        Some(body.clone()),
                    )
                }
                EventKind::Inline { path, body } => (
                    "◆",
                    DIM,
                    vec![
                        actor,
                        Span::styled(" commented on ", dim),
                        Span::raw(path.clone()),
                    ],
                    Some(body.clone()),
                ),
                EventKind::Merged => (
                    "●",
                    PURPLE,
                    vec![Span::styled(
                        "Merged",
                        Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
                    )],
                    None,
                ),
                EventKind::Closed => (
                    "●",
                    closed_color,
                    vec![Span::styled(
                        closed,
                        Style::default()
                            .fg(closed_color)
                            .add_modifier(Modifier::BOLD),
                    )],
                    None,
                ),
            };
        // The picked comment stands out: a pointer for its glyph, a thick
        // blue rail down its body, and a reminder of what enter does, but
        // only when that fits without cutting the header's own words.
        let (left, right) = if picked {
            let mut left = vec![Span::styled("▶  ", accent)];
            left.extend(what);
            let hint = [Span::styled(REPLY_HINT, accent), Span::styled(" · ", dim)];
            let needed: usize = left
                .iter()
                .chain(&hint)
                .chain(std::iter::once(&when))
                .map(|s| s.content.width())
                .sum();
            let right = if needed < width {
                hint.into_iter().chain(std::iter::once(when)).collect()
            } else {
                vec![when]
            };
            (left, right)
        } else {
            let mut left = vec![Span::styled(
                format!("{glyph}  "),
                Style::default().fg(color),
            )];
            left.extend(what);
            (left, vec![when])
        };
        lines.push(spread(left, right, width));
        if let Some(body) = body.map(|b| pr::visible(&b)).filter(|b| !b.is_empty()) {
            let body = if quotes { body } else { without_quotes(&body) };
            let room = (width as u16).saturating_sub(indent);
            // A cut-short excerpt would cut a picture in two, so pictures
            // show only where comments are shown whole.
            let rendered = if expanded && gallery.shows_media() {
                let laid = with_media(&body, room, indent, gallery);
                let top = lines.len();
                figures.extend(laid.figures.into_iter().map(|mut figure| {
                    figure.top += top;
                    figure
                }));
                laid.lines
            } else {
                markdown(&body, room, indent)
            };
            let total = rendered.len();
            let shown = if expanded { total } else { total.min(EXCERPT) };
            lines.extend(rendered.into_iter().take(shown).map(|mut line| {
                line.spans
                    .insert(0, Span::styled(if picked { "┃" } else { "│" }, rail));
                line.spans[1] = Span::styled(
                    line.spans[1].content.chars().skip(1).collect::<String>(),
                    line.spans[1].style,
                );
                line
            }));
            if shown < total {
                lines.push(Line::from(vec![
                    Span::styled(if picked { "┃" } else { "│" }, rail),
                    Span::styled(
                        format!("   … {} more lines · e expands", total - shown),
                        dim,
                    ),
                ]));
            }
        }
        // The picked comment's rail runs on to the next event.
        lines.push(Line::from(Span::styled(
            if picked { "┃" } else { "│" },
            rail,
        )));
    }
    lines.pop();
    Listed {
        lines,
        starts,
        figures,
    }
}

/// Swaps markdown and HTML images for `▣ alt` so they don't leave blank space.
fn without_images(text: &str) -> String {
    use std::sync::LazyLock;

    use regex::Regex;

    static MARKDOWN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"!\[([^\]]*)\]\([^)]*\)").expect("pattern is valid"));
    static HTML: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)<img\b[^>]*>").expect("pattern is valid"));
    static ALT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"(?i)\balt="([^"]*)""#).expect("pattern is valid"));
    let text = MARKDOWN.replace_all(text, |c: &regex::Captures| {
        let alt = c.get(1).map_or("", |m| m.as_str());
        format!("▣ {}", if alt.is_empty() { "image" } else { alt })
    });
    HTML.replace_all(&text, |c: &regex::Captures| {
        let alt = ALT
            .captures(c.get(0).map_or("", |m| m.as_str()))
            .and_then(|a| a.get(1))
            .map_or("image", |m| m.as_str());
        format!("▣ {}", if alt.is_empty() { "image" } else { alt })
    })
    .into_owned()
}

/// Markdown laid out by the reader, as lines `indent` columns in. Images
/// become a short placeholder: used where pictures can't be drawn.
fn markdown(text: &str, width: u16, indent: u16) -> Vec<Line<'static>> {
    let text = without_images(text);
    let laid = render::layout(&text, width.max(10), (8, 16), &|_| None);
    let mut lines = rows(&laid.rows, indent);
    while lines
        .last()
        .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_refresh_closes_the_log_when_its_check_is_gone() {
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Kind::Pull,
            Load::Ready(detail(State::Open)),
        );
        assert_eq!(
            page.picked_check().map(|c| c.label()),
            Some("CI / test".into())
        );
        page.log = Some(34);
        let mut next = detail(State::Open);
        next.checks.retain(|c| c.name != "test");
        next.checks.push(pr::Check {
            name: "lint".into(),
            workflow: "CI".into(),
            status: CheckStatus::Failed,
            url: None,
            started: String::new(),
            completed: String::new(),
            run_id: Some(12),
            job_id: Some(99),
        });
        assert_eq!(page.refreshed(next), None);
        assert_eq!(
            page.log, None,
            "the log doesn't move to another check's job"
        );
    }

    #[test]
    fn a_failed_step_opens_the_group_its_last_error_is_in() {
        let line = |text: &str, kind| pr::LogLine {
            text: text.into(),
            kind,
        };
        let log = pr::JobLog {
            steps: vec![pr::Step {
                name: "Run tests".into(),
                status: CheckStatus::Failed,
                queued: false,
                started: String::new(),
                completed: String::new(),
                items: vec![
                    pr::LogItem::Line(line("starting", pr::LineKind::Plain)),
                    pr::LogItem::Group {
                        title: "Run cargo test".into(),
                        lines: vec![
                            line("running 3 tests", pr::LineKind::Plain),
                            line("test failed", pr::LineKind::Error),
                        ],
                    },
                    pr::LogItem::Line(line("done", pr::LineKind::Plain)),
                ],
                earlier: 0,
            }],
            done: true,
            missing: None,
        };
        let (steps, groups, pick) = job_opening(&log);
        assert_eq!(steps, HashSet::from([0]));
        assert_eq!(groups, HashSet::from([(0, 1)]));
        assert_eq!(pick, Fold::Step(0));
    }

    #[test]
    fn a_job_log_fits_a_narrow_pane() {
        let steps = vec![pr::Step {
            name: "Run tests".into(),
            status: CheckStatus::Failed,
            queued: false,
            started: String::new(),
            completed: String::new(),
            items: (0..120)
                .map(|n| {
                    pr::LogItem::Line(pr::LogLine {
                        text: format!("line {n} with words that need wrapping"),
                        kind: pr::LineKind::Plain,
                    })
                })
                .collect(),
            earlier: 0,
        }];
        let mut quiet = steps.clone();
        quiet[0].items.clear();
        let job = Load::Ready(pr::JobLog {
            steps,
            done: true,
            missing: None,
        });
        let missing = Load::Ready(pr::JobLog {
            steps: quiet,
            done: true,
            missing: Some(
                "Couldn't read this job's log yet; trying again every few seconds. o opens it on GitHub. (HTTP 503)"
                    .into(),
            ),
        });
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Kind::Pull,
            Load::Loading,
        );
        page.open_steps.insert(0);
        for job in [&job, &missing] {
            for width in [12, 16, 24, 40] {
                let view = job_view(None, Some(job), &page, width, 0);
                for line in &view.lines {
                    assert!(
                        line.width() <= width,
                        "{width} columns: {:?} is {} wide",
                        plain(line),
                        line.width()
                    );
                }
            }
        }
        // Wrapped, not cut: the whole note is still there.
        let view = job_view(None, Some(&missing), &page, 24, 0);
        let text: String = view.lines.iter().map(|l| plain(l)).collect();
        assert!(text.contains("GitHub."), "{text}");
    }

    #[test]
    fn a_refresh_follows_the_picked_check_to_its_new_job() {
        let check = |job_id| pr::Check {
            name: "test".into(),
            workflow: "CI".into(),
            status: CheckStatus::Running,
            url: None,
            started: String::new(),
            completed: String::new(),
            run_id: Some(1),
            job_id,
        };
        assert_eq!(job_after_refresh(Some(&check(Some(10)))), Some(10));
        assert_eq!(job_after_refresh(Some(&check(Some(11)))), Some(11));
        assert_eq!(job_after_refresh(Some(&check(None))), None);
        assert_eq!(job_after_refresh(None), None);
    }

    use super::*;
    use crate::pr::{Check, MergeMethod, Reviewer};

    fn detail(state: State) -> Detail {
        Detail {
            pr: PrRef {
                repo: "o/r".into(),
                number: 7,
            },
            kind: Kind::Pull,
            assignees: Vec::new(),
            title: "fix: Find curly quotes and Unicode hyphens from typed ones".into(),
            body: "Searching `don't` skips **every** `don’t`.\n\n![Before](https://x/a.png)".into(),
            state,
            author: "marcos".into(),
            updated: "2026-11-12T04:00:00Z".into(),
            base: "main".into(),
            head: "fix/find".into(),
            head_sha: "abc".into(),
            additions: 57,
            deletions: 4,
            labels: vec!["size:M".into()],
            reviewers: vec![Reviewer {
                login: "ann".into(),
                verdict: Verdict::Approved,
            }],
            checks: vec![
                Check {
                    name: "CodeRabbit".into(),
                    workflow: String::new(),
                    status: CheckStatus::Passed,
                    url: None,
                    started: String::new(),
                    completed: String::new(),
                    run_id: None,
                    job_id: None,
                },
                Check {
                    name: "test".into(),
                    workflow: "CI".into(),
                    status: CheckStatus::Failed,
                    url: None,
                    started: String::new(),
                    completed: String::new(),
                    run_id: Some(12),
                    job_id: Some(34),
                },
            ],
            mergeability: Mergeability::Ready,
            methods: vec![MergeMethod::Squash, MergeMethod::Merge],
            can_write: true,
            media: Default::default(),
            timeline: vec![
                pr::Event {
                    id: None,
                    at: "2026-11-12T03:00:00Z".into(),
                    actor: "marcos".into(),
                    kind: EventKind::Opened,
                },
                pr::Event {
                    id: None,
                    at: "2026-11-12T03:30:00Z".into(),
                    actor: "bot".into(),
                    kind: EventKind::Comment {
                        body: (1..=30).map(|n| format!("line {n}\n\n")).collect(),
                    },
                },
            ],
        }
    }

    fn app_with_page(mut detail: Detail) -> App {
        detail.media.clear();
        let (tx, rx) = mpsc::channel();
        let page = Page::new(detail.pr.clone(), detail.kind, Load::Ready(detail.clone()));
        App {
            tx,
            rx,
            look: Look::detect(),
            news: None,
            clock: usage::Clock::start(),
            here: None,
            branch: None,
            list: None,
            page: Some(page),
            details: HashMap::from([(detail.pr.clone(), (detail, Instant::now()))]),
            inflight: HashSet::new(),
            prefetched: HashSet::new(),
            patches: HashMap::new(),
            patching: HashSet::new(),
            logs: Logs::default(),
            gallery: Gallery::plain(),
            now: 0,
        }
    }

    #[test]
    fn live_refresh_polls_ready_prs_and_issues_every_ten_seconds() {
        for kind in [Kind::Pull, Kind::Issue] {
            let mut next = detail(State::Open);
            next.kind = kind;
            next.checks.clear();
            let mut app = app_with_page(next);
            let before = Instant::now() - Duration::from_secs(11);
            let page = app.page.as_mut().unwrap();
            page.polled = before;
            let pr = page.pr.clone();
            app.details.get_mut(&pr).unwrap().1 = before;
            app.inflight.insert(pr);
            app.poll();
            let asked = app.page.as_ref().unwrap().polled;
            assert!(
                asked > before,
                "ready {kind:?} wasn't polled after ten seconds"
            );
            app.poll();
            assert_eq!(app.page.as_ref().unwrap().polled, asked);
        }
    }

    #[test]
    fn live_refresh_waits_for_page_interaction_to_finish() {
        for blocked in 0..5 {
            let mut next = detail(State::Open);
            next.checks[0].status = CheckStatus::Running;
            let mut app = app_with_page(next);
            let before = Instant::now() - Duration::from_secs(21);
            let page = app.page.as_mut().unwrap();
            page.polled = before;
            match blocked {
                0 => {
                    page.mode = Mode::Compose {
                        text: "draft".chars().collect(),
                        cursor: 5,
                        reply: None,
                        refused: None,
                    }
                }
                1 => page.mode = Mode::Help,
                2 => {
                    page.selection = Some(Selection {
                        anchor: (0, 0),
                        head: (0, 2),
                    })
                }
                3 => page.dragging = true,
                _ => page.busy = Some("Posting comment…".into()),
            }
            let pr = page.pr.clone();
            app.details.get_mut(&pr).unwrap().1 = before;
            app.inflight.insert(pr);
            app.poll();
            assert_eq!(
                app.page.as_ref().unwrap().polled,
                before,
                "interaction {blocked}"
            );
        }
    }

    #[test]
    fn live_refresh_defers_arrivals_and_preserves_the_reply_target() {
        for composing in [true, false] {
            let mut app = app_with_page(detail(State::Open));
            let page = app.page.as_mut().unwrap();
            page.show(Tab::Comments);
            if composing {
                page.reply();
            } else {
                page.selection = Some(Selection {
                    anchor: (0, 0),
                    head: (0, 2),
                });
            }
            let mut next = detail(State::Open);
            next.title = "updated".into();
            next.timeline.insert(1, next.timeline[0].clone());
            page.refreshed(next);
            let Load::Ready(shown) = &page.detail else {
                panic!("expected detail")
            };
            assert_ne!(shown.title, "updated");
            assert_eq!(page.focus, Some(1));
            if composing {
                assert!(
                    matches!(&page.mode, Mode::Compose { reply: Some(actor), .. } if actor == "bot")
                );
            }
            page.mode = Mode::Normal;
            page.selection = None;
            app.poll();
            let page = app.page.as_ref().unwrap();
            assert!(page.pending.is_none());
            assert_eq!(page.focus, Some(2));
            let Load::Ready(shown) = &page.detail else {
                panic!("expected detail")
            };
            assert_eq!(shown.title, "updated");
            assert_eq!(shown.timeline[page.focus.unwrap()].actor, "bot");
        }
    }

    #[test]
    fn live_refresh_keeps_edited_reply_targets_when_timeline_reorders() {
        for (before, after) in [
            (
                EventKind::Comment {
                    body: "before".into(),
                },
                EventKind::Comment {
                    body: "after".into(),
                },
            ),
            (
                EventKind::Review {
                    verdict: Verdict::Commented,
                    body: "before".into(),
                },
                EventKind::Review {
                    verdict: Verdict::Dismissed,
                    body: "after".into(),
                },
            ),
            (
                EventKind::Inline {
                    path: "src/pr.rs".into(),
                    body: "before".into(),
                },
                EventKind::Inline {
                    path: "src/pr.rs".into(),
                    body: "after".into(),
                },
            ),
        ] {
            for id in [None, Some("reply-id")] {
                let mut old = detail(State::Open);
                old.timeline[1].id = id.map(str::to_owned);
                old.timeline[1].kind = before.clone();
                let mut next = old.clone();
                next.timeline[1].kind = after.clone();
                next.timeline.swap(0, 1);
                let mut page = Page::new(old.pr.clone(), old.kind, Load::Ready(old));
                page.show(Tab::Comments);
                page.refreshed(next);
                assert_eq!(
                    page.focus,
                    Some(0),
                    "an edit must not clear the reply target"
                );
                page.reply();
                let Mode::Compose { text, reply, .. } = &page.mode else {
                    panic!("expected the composer");
                };
                assert_eq!(text.iter().collect::<String>(), "> after\n\n");
                assert_eq!(reply.as_deref(), Some("bot"));
            }
        }
    }

    #[test]
    fn live_refresh_uses_ids_for_colliding_comments_and_drops_deleted_targets() {
        for kind in [Kind::Pull, Kind::Issue] {
            let mut old = detail(State::Open);
            old.kind = kind;
            old.timeline[1].id = Some("IC_picked".into());
            let mut other = old.timeline[1].clone();
            other.id = Some("IC_other".into());
            old.timeline.push(other);
            let mut next = old.clone();
            next.timeline.swap(1, 2);
            next.timeline[2].kind = EventKind::Comment {
                body: "edited".into(),
            };
            let mut page = Page::new(old.pr.clone(), kind, Load::Ready(old));
            page.show(Tab::Comments);
            page.refreshed(next.clone());
            assert_eq!(page.focus, Some(2));
            next.timeline.remove(2);
            page.refreshed(next);
            assert_eq!(
                page.focus, None,
                "must not switch to the other comment's ID"
            );
        }
    }

    #[test]
    fn live_refresh_drops_ambiguous_reply_targets() {
        for duplicate_before in [true, false] {
            let mut old = detail(State::Open);
            if duplicate_before {
                old.timeline.push(old.timeline[1].clone());
            }
            let mut next = old.clone();
            if duplicate_before {
                next.timeline.remove(1);
            } else {
                next.timeline.push(next.timeline[1].clone());
            }
            let mut page = Page::new(old.pr.clone(), old.kind, Load::Ready(old));
            page.show(Tab::Comments);
            page.refreshed(next);
            assert_eq!(page.focus, None, "ambiguous before: {duplicate_before}");
        }
    }

    #[test]
    fn live_refresh_keeps_the_five_second_poll_for_shown_running_logs() {
        let mut app = app_with_page(detail(State::Open));
        let before = Instant::now() - Duration::from_secs(6);
        let page = app.page.as_mut().unwrap();
        page.tab = Tab::Checks;
        page.log = Some(34);
        page.log_polled = before;
        app.logs.jobs.insert(
            34,
            Load::Ready(pr::JobLog {
                steps: Vec::new(),
                done: false,
                missing: None,
            }),
        );
        app.logs.inflight.insert(34);
        app.poll();
        let asked = app.page.as_ref().unwrap().log_polled;
        assert!(asked > before);
        app.poll();
        assert_eq!(app.page.as_ref().unwrap().log_polled, asked);
        let page = app.page.as_mut().unwrap();
        page.tab = Tab::Summary;
        page.log_polled = before;
        app.poll();
        assert_eq!(app.page.as_ref().unwrap().log_polled, before);
    }

    #[test]
    fn the_composer_shows_only_its_own_refusal() {
        let draw = |page: &Page| {
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 10))
                .expect("test terminal");
            terminal
                .draw(|f| footer(f, page, f.area(), 0, 100))
                .expect("draws");
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        + "\n"
                })
                .collect::<String>()
        };
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Kind::Pull,
            Load::Loading,
        );
        page.mode = Mode::Compose {
            text: Vec::new(),
            cursor: 0,
            reply: None,
            refused: None,
        };
        page.toast = Some("A background refresh failed".into());
        let text = draw(&page);
        assert!(text.contains("enter send"), "{text}");
        assert!(!text.contains("background refresh"), "{text}");
        if let Mode::Compose { refused, .. } = &mut page.mode {
            *refused = empty_comment("", false);
        }
        assert!(draw(&page).contains("Can't post an empty comment"));
    }

    #[test]
    fn empty_comments_and_quote_only_replies_are_not_posted() {
        assert!(empty_comment("", false).is_some());
        assert!(empty_comment("", true).is_some());
        assert!(empty_comment("> what bot said", true).is_some());
        assert!(empty_comment(">\n> two lines", true).is_some());
        assert_eq!(empty_comment("> what bot said\n\nthanks", true), None);
        assert_eq!(empty_comment("thanks", false), None);
        // A comment of your own may be a quote alone.
        assert_eq!(empty_comment("> from the docs", false), None);
    }

    #[test]
    fn a_finished_job_whose_log_failed_is_asked_again_while_shown() {
        let mut app = app_with_page(detail(State::Open));
        let before = Instant::now() - Duration::from_secs(6);
        let page = app.page.as_mut().unwrap();
        page.tab = Tab::Checks;
        page.log = Some(34);
        page.log_polled = before;
        app.logs.jobs.insert(
            34,
            Load::Ready(pr::JobLog {
                steps: Vec::new(),
                done: true,
                missing: Some("HTTP 503".into()),
            }),
        );
        app.logs.inflight.insert(34);
        app.poll();
        assert!(app.page.as_ref().unwrap().log_polled > before);
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| plain(l).trim_end().to_owned())
            .collect()
    }

    #[test]
    fn the_comments_tab_holds_only_comments_in_full() {
        let now = pr::seconds("2026-11-12T04:30:00Z").unwrap();
        let mut d = detail(State::Open);
        d.timeline.push(pr::Event {
            id: None,
            at: "2026-11-12T04:00:00Z".into(),
            actor: "ana".into(),
            kind: EventKind::Commit {
                sha: "abc1234".into(),
                headline: "fix it".into(),
            },
        });
        let Listed { lines, starts, .. } = comments(&d, 60, now, None, true, &mut Gallery::plain());
        let shown = texts(&lines);
        assert!(shown[0].starts_with("◆  bot commented"), "{shown:?}");
        // Picked, it's pointed at, its rail is thick, and enter is named.
        let picked = comments(&d, 60, now, Some(1), true, &mut Gallery::plain()).lines;
        let picked = texts(&picked);
        assert!(picked[0].starts_with("▶  bot commented"), "{picked:?}");
        assert!(picked[0].contains("enter replies"), "{picked:?}");
        assert!(picked[1].starts_with('┃'), "{picked:?}");
        // Too narrow for the hint, the header keeps its own words whole.
        let narrow = comments(&d, 24, now, Some(1), true, &mut Gallery::plain()).lines;
        let narrow = texts(&narrow);
        assert!(narrow[0].starts_with("▶  bot commented"), "{narrow:?}");
        assert!(!narrow[0].contains("enter"), "{narrow:?}");
        assert!(shown.iter().any(|l| l.contains("line 30")));
        assert!(
            !shown
                .iter()
                .any(|l| l.contains("opened") || l.contains("abc1234"))
        );
        assert_eq!(starts.len(), d.timeline.len());
        assert_eq!(starts[1], 0);

        d.timeline.truncate(1);
        let lines = comments(&d, 60, now, None, true, &mut Gallery::plain()).lines;
        assert!(texts(&lines)[0].starts_with("No comments on this pull request yet"));
    }

    #[test]
    fn tabs_step_from_comments_to_code_to_checks_for_a_pull_request_only() {
        let pr = PrRef {
            repo: "o/r".into(),
            number: 1,
        };
        let mut page = Page::new(pr.clone(), Kind::Pull, Load::Ready(detail(State::Open)));
        assert_eq!(page.step(1, false), Step::Tab(Tab::Timeline));
        assert_eq!(page.step(-1, false), Step::Stay);
        assert_eq!(page.step(-1, true), Step::Tab(Tab::Checks));
        page.show(Tab::Comments);
        // Opening Comments picks the first comment, ready to reply to.
        assert_eq!(page.focus, Some(1));
        assert_eq!(page.step(1, false), Step::Code);
        page.show(Tab::Checks);
        assert_eq!(page.step(-1, false), Step::Code);
        assert_eq!(page.step(1, false), Step::Stay);

        let mut issue = Page::new(pr, Kind::Issue, Load::Ready(detail(State::Open)));
        issue.show(Tab::Comments);
        assert_eq!(issue.step(1, false), Step::Stay);
        assert_eq!(issue.step(1, true), Step::Tab(Tab::Summary));
        issue.pick(1);
        assert_eq!(issue.tab, Tab::Comments, "picking stays on Comments");
    }

    #[test]
    fn replying_quotes_only_the_picked_comment() {
        let mut d = detail(State::Open);
        d.timeline.push(pr::Event {
            id: None,
            at: "2026-11-12T04:00:00Z".into(),
            actor: "ana".into(),
            kind: EventKind::Comment {
                body: "Second thought".into(),
            },
        });
        let pr = PrRef {
            repo: "o/r".into(),
            number: 1,
        };
        let mut page = Page::new(pr, Kind::Pull, Load::Ready(d));
        page.show(Tab::Comments);
        page.pick(1);
        assert_eq!(page.focus, Some(2));
        page.reply();
        let Mode::Compose { text, reply, .. } = &page.mode else {
            panic!("expected the composer");
        };
        assert_eq!(text.iter().collect::<String>(), "> Second thought\n\n");
        assert_eq!(reply.as_deref(), Some("ana"));
    }

    #[test]
    fn the_summary_shows_only_what_has_something_to_say() {
        let body = summary(&detail(State::Open), 80, &mut Gallery::plain());
        let text = texts(&body.lines).join("\n");
        assert!(text.contains("Status     Ready to merge"), "{text}");
        assert!(text.contains("Branch     fix/find → main"), "{text}");
        assert!(text.contains("Reviewers  ✓ ann"), "{text}");
        assert!(text.contains("Labels     size:M"), "{text}");
        assert!(text.contains("✗ CI / test"), "{text}");
        assert!(
            !text.contains("CodeRabbit"),
            "passing checks stay out: {text}"
        );
        assert!(
            text.contains("Searching don't skips every don’t."),
            "{text}"
        );
        assert!(text.contains("▣ Before"), "without Kitty graphics: {text}");
        assert!(body.figures.is_empty());

        let mut bare = detail(State::Open);
        bare.reviewers.clear();
        bare.labels.clear();
        let text = texts(&summary(&bare, 80, &mut Gallery::plain()).lines).join("\n");
        assert!(
            !text.contains("Reviewers") && !text.contains("Labels"),
            "{text}"
        );
    }

    #[test]
    fn long_timeline_comments_are_cut_short_until_expanded() {
        let now = pr::seconds("2026-11-12T04:30:00Z").unwrap();
        let short = texts(
            &timeline(
                &detail(State::Open),
                60,
                now,
                false,
                None,
                &mut Gallery::plain(),
            )
            .lines,
        );
        assert!(short[0].starts_with("●  marcos opened this pull request"));
        assert!(short.iter().any(|l| l.contains("more lines · e expands")));
        let long = texts(
            &timeline(
                &detail(State::Open),
                60,
                now,
                true,
                None,
                &mut Gallery::plain(),
            )
            .lines,
        );
        assert!(long.iter().any(|l| l.contains("line 30")));
        assert!(!long.iter().any(|l| l.contains("more lines")));
    }

    #[test]
    fn actions_follow_the_state() {
        assert!(matches!(
            available(&detail(State::Open), 'm'),
            Ok(Next::Merge)
        ));
        assert!(available(&detail(State::Draft), 'm').is_err());
        assert!(available(&detail(State::Merged), 'm').is_err());
        assert!(matches!(
            available(&detail(State::Closed), 'x'),
            Ok(Next::Confirm(Action::Reopen))
        ));
        assert!(matches!(
            available(&detail(State::Draft), 'r'),
            Ok(Next::Now(Action::Ready))
        ));
        let mut conflicted = detail(State::Open);
        conflicted.mergeability = Mergeability::Conflicts;
        assert!(available(&conflicted, 'm').is_err());
        let mut reader = detail(State::Open);
        reader.can_write = false;
        assert!(available(&reader, 'm').is_err());
    }

    #[test]
    fn the_list_shows_each_row_once_per_tab_and_filters() {
        let entry = |number, title: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number,
            },
            title: title.into(),
            author: "me".into(),
            updated: format!("2026-11-12T04:0{number}:00Z"),
            draft: false,
            ended: None,
        };
        let mut list = list_of(vec![
            Slot {
                source: Source::Authored(Kind::Pull),
                status: Status::Open,
                entries: Some(vec![entry(1, "fix quotes"), entry(2, "add pr")]),
                fetch: Fetch::Done,
                background: false,
            },
            Slot {
                source: Source::Reviewing,
                status: Status::Open,
                entries: Some(vec![entry(2, "add pr")]),
                fetch: Fetch::Loading,
                background: false,
            },
            Slot {
                source: Source::Authored(Kind::Issue),
                status: Status::Open,
                entries: Some(vec![entry(3, "crash on open")]),
                fetch: Fetch::Done,
                background: false,
            },
            Slot {
                source: Source::Authored(Kind::Issue),
                status: Status::Done,
                entries: Some(vec![entry(4, "old crash"), entry(5, "older crash")]),
                fetch: Fetch::Idle,
                background: false,
            },
        ]);
        assert_eq!(list.count(), 2);
        // Newest first, across searches.
        assert_eq!(list.picked().map(|pr| pr.number), Some(2));
        assert!(list.loading());
        list.typed = "quotes".into();
        assert_eq!(list.picked().map(|pr| pr.number), Some(1));
        list.typed.clear();
        list.show(Kind::Issue);
        assert_eq!(list.count(), 1);
        assert!(!list.loading());
        assert_eq!(list.picked().map(|pr| pr.number), Some(3));
        // The done switch shows the other searches, and the tab stays.
        list.selected = 0;
        list.show_status(Status::Done);
        assert_eq!(list.kind, Kind::Issue);
        assert_eq!(list.count(), 2);
        assert_eq!(list.picked().map(|pr| pr.number), Some(5));
    }

    fn list_of(slots: Vec<Slot>) -> List {
        List {
            kind: Kind::Pull,
            status: Status::Open,
            scope: Scope::All,
            slots,
            me: Some("me".into()),
            tab_hits: Vec::new(),
            typed: String::new(),
            selected: 0,
            resting: None,
            hits: Vec::new(),
            pressed: None,
            note: None,
            shown: Vec::new(),
            shown_area: Rect::default(),
            selection: None,
            dragging: false,
            polled: Instant::now(),
            pending: Vec::new(),
        }
    }

    fn live_rows() -> Vec<Slot> {
        [Status::Open, Status::Done]
            .into_iter()
            .flat_map(|status| {
                [
                    Source::Authored(Kind::Pull),
                    Source::Reviewing,
                    Source::Authored(Kind::Issue),
                ]
                .into_iter()
                .map(move |source| Slot {
                    source,
                    status,
                    entries: Some(vec![live_row(1), live_row(2)]),
                    fetch: Fetch::Done,
                    background: false,
                })
            })
            .collect()
    }

    fn live_row(number: u64) -> Entry {
        Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number,
            },
            title: format!("row {number}"),
            author: "me".into(),
            updated: format!("2026-11-12T04:0{number}:00Z"),
            draft: false,
            ended: None,
        }
    }

    #[test]
    fn live_refresh_polls_only_shown_list_searches_and_throttles_failures() {
        for kind in [Kind::Pull, Kind::Issue] {
            for status in [Status::Open, Status::Done] {
                let mut list = list_of(live_rows());
                list.kind = kind;
                list.status = status;
                let picked = list.picked();
                let before = Instant::now() - Duration::from_secs(11);
                list.polled = before;
                let asks = list.poll();
                assert_eq!(asks.len(), if kind == Kind::Pull { 2 } else { 1 });
                assert!(list.polled > before);
                assert_eq!(list.picked(), picked);
                for slot in &list.slots {
                    assert_eq!(
                        matches!(slot.fetch, Fetch::Loading),
                        slot.source.kind() == kind && slot.status == status
                    );
                }
                for (index, source, asked_status) in asks {
                    assert_eq!(source.kind(), kind);
                    assert_eq!(asked_status, status);
                    list.searched(index, Err("offline".into()));
                }
                assert_eq!(list.picked(), picked);
                assert_eq!(list.count(), 2);
                assert!(list.poll().is_empty());
                list.polled = Instant::now() - Duration::from_secs(9);
                assert!(list.poll().is_empty());
                list.polled = before;
                assert!(!list.poll().is_empty());
            }
        }
    }

    #[test]
    fn assigned_does_not_restart_a_search_still_out_of_scope() {
        let mut list = list_of(live_rows());
        list.scope = Scope::Assigned;
        list.polled = Instant::now() - Duration::from_secs(11);
        let asks = list.poll();
        assert!(!asks.is_empty());
        list.polled = Instant::now() - Duration::from_secs(11);
        assert!(
            list.poll().is_empty(),
            "the searches outside Assigned are still out"
        );
    }

    #[test]
    fn live_refresh_skips_filtered_or_selected_lists_and_hidden_lists() {
        for blocked in 0..3 {
            let mut list = list_of(live_rows());
            let before = Instant::now() - Duration::from_secs(11);
            list.polled = before;
            match blocked {
                0 => list.typed = "row".into(),
                1 => list.dragging = true,
                _ => {
                    list.selection = Some(Selection {
                        anchor: (0, 0),
                        head: (0, 2),
                    })
                }
            }
            assert!(list.poll().is_empty());
            assert_eq!(list.polled, before);
        }
        let mut app = app_with_page(detail(State::Open));
        let mut list = list_of(live_rows());
        let before = Instant::now() - Duration::from_secs(11);
        list.polled = before;
        app.list = Some(list);
        app.poll();
        let list = app.list.as_ref().unwrap();
        assert_eq!(list.polled, before);
        assert!(!list.loading());
    }

    #[test]
    fn live_refresh_keeps_rows_while_filtering() {
        let mut list = list_of(vec![live_rows().remove(0)]);
        list.polled -= POLL;
        assert_eq!(list.poll().len(), 1);
        list.typed = "row".into();
        list.searched(0, Ok(vec![live_row(1), live_row(2), live_row(3)]));
        assert_eq!(list.count(), 2);
        assert!(list.poll().is_empty());
        assert_eq!(list.count(), 2);
        list.typed.clear();
        assert!(list.poll().is_empty());
        assert_eq!(list.count(), 3);
        list.typed = "row".into();
        list.reload(false);
        list.searched(0, Ok(vec![live_row(1), live_row(2)]));
        assert_eq!(
            list.count(),
            2,
            "manual reload still updates filtered results"
        );
    }

    #[test]
    fn live_refresh_keeps_picked_identity_when_rows_reorder_after_a_drag() {
        let mut list = list_of(vec![live_rows().remove(0)]);
        let picked = list.picked();
        list.dragging = true;
        list.selection = Some(Selection {
            anchor: (0, 0),
            head: (0, 2),
        });
        list.slots[0].fetch = Fetch::Loading;
        list.searched(0, Ok(vec![live_row(1), live_row(2), live_row(3)]));
        assert!(
            !list.loading(),
            "arrival clears inflight state even while deferred"
        );
        assert_eq!(list.count(), 2);
        assert_eq!(list.picked(), picked);
        list.dragging = false;
        assert!(list.poll().is_empty());
        assert_eq!(list.count(), 2);
        list.selection = None;
        assert!(list.poll().is_empty());
        assert_eq!(list.count(), 3);
        assert_eq!(list.picked(), picked);
        assert_eq!(list.selected, 1);
        assert!(list.pending.is_empty());
        list.searched(0, Ok(vec![live_row(1), live_row(2)]));
        assert_eq!(list.selected, 0);
        assert_eq!(list.picked(), picked);
    }

    #[test]
    fn live_refresh_preserves_manual_reload_scope() {
        let mut list = list_of(live_rows());
        list.slots[3].fetch = Fetch::Idle;
        list.slots[4].fetch = Fetch::Failed("offline".into());
        list.slots[5].fetch = Fetch::Idle;
        let asks = list.reload(false);
        assert_eq!(
            asks.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            [0, 1, 2, 4]
        );
        assert!(matches!(list.slots[3].fetch, Fetch::Idle));
        assert!(matches!(list.slots[5].fetch, Fetch::Idle));
        assert_eq!(list.count(), 2);
    }

    #[test]
    fn done_rows_are_grouped_by_when_they_ended_with_a_sparkline() {
        let now = pr::seconds("2026-11-14T12:00:00Z").unwrap();
        let done = |number, title: &str, how, at: &str| Entry {
            pr: PrRef {
                repo: "o/leaf".into(),
                number,
            },
            title: title.into(),
            author: "me".into(),
            updated: at.into(),
            draft: false,
            ended: Some(Ended { how, at: at.into() }),
        };
        let mut list = list_of(vec![Slot {
            source: Source::Authored(Kind::Pull),
            status: Status::Done,
            entries: Some(vec![
                done(1, "ship it", Ending::Merged, "2026-11-14T09:00:00Z"),
                done(2, "fix quotes", Ending::Merged, "2026-11-13T09:00:00Z"),
                done(3, "old work", Ending::Merged, "2026-08-01T09:00:00Z"),
            ]),
            fetch: Fetch::Done,
            background: false,
        }]);
        list.status = Status::Done;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24))
            .expect("test terminal");
        terminal
            .draw(|f| draw_list(f, &mut list, now, None))
            .expect("draws");
        let buffer = terminal.backend().buffer();
        let screen: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let text = screen.join("\n");
        assert!(text.contains("Open  ·  Merged"), "{text}");
        assert!(text.contains("2 merged in 12 weeks"), "{text}");
        let at = |needle: &str| screen.iter().position(|l| l.contains(needle));
        assert!(at("Today") < at("ship it"), "{text}");
        assert!(at("ship it") < at("Yesterday"), "{text}");
        assert!(at("Yesterday") < at("fix quotes"), "{text}");
        assert!(at("This year") < at("old work"), "{text}");
        assert!(text.contains("◆  ship it"), "{text}");
        assert!(text.contains("me · merged"), "{text}");
        assert!(text.contains("tab open"), "{text}");
        // The switch can be clicked.
        assert!(
            list.tab_hits
                .iter()
                .any(|(_, hit)| *hit == ListHit::Status(Status::Open))
        );
    }

    #[test]
    fn assigned_shows_only_what_is_assigned_to_you() {
        let entry = |number: u64, title: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number,
            },
            title: title.into(),
            author: "someone".into(),
            updated: format!("2026-11-12T04:0{number}:00Z"),
            draft: false,
            ended: None,
        };
        let slot = |source: Source, entries: Vec<Entry>| Slot {
            source,
            status: Status::Open,
            entries: Some(entries),
            fetch: Fetch::Done,
            background: false,
        };
        let mut list = list_of(vec![
            slot(Source::Authored(Kind::Pull), vec![entry(1, "mine")]),
            slot(Source::Assigned(Kind::Pull), vec![entry(2, "handed to me")]),
            slot(Source::Assigned(Kind::Issue), Vec::new()),
        ]);
        let titles =
            |list: &List| -> Vec<String> { list.entries().into_iter().map(|e| e.title).collect() };
        assert_eq!(titles(&list), ["handed to me", "mine"]);
        list.show_scope(Scope::Assigned);
        assert_eq!(titles(&list), ["handed to me"]);
        assert_eq!(list.all(Kind::Issue).len(), 0);

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24))
            .expect("test terminal");
        list.kind = Kind::Issue;
        terminal
            .draw(|f| draw_list(f, &mut list, 0, None))
            .expect("draws");
        let buffer = terminal.backend().buffer();
        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(
            text.contains("All  ·  Assigned     Open  ·  Closed"),
            "{text}"
        );
        assert!(
            text.contains("No open issues are assigned to you."),
            "{text}"
        );
        assert!(text.contains("@ all"), "{text}");
        assert!(
            list.tab_hits
                .iter()
                .any(|(_, hit)| *hit == ListHit::Scope(Scope::All))
        );
    }

    #[test]
    fn rows_fall_into_age_groups_and_weeks() {
        let now = pr::seconds("2026-11-14T12:00:00Z").unwrap();
        assert_eq!(age_group("2026-11-14T00:01:00Z", now), AgeGroup::Today);
        assert_eq!(age_group("2026-11-13T23:59:00Z", now), AgeGroup::Yesterday);
        assert_eq!(age_group("2026-11-09T12:00:00Z", now), AgeGroup::ThisWeek);
        assert_eq!(age_group("2026-10-20T12:00:00Z", now), AgeGroup::ThisMonth);
        assert_eq!(age_group("2026-01-20T12:00:00Z", now), AgeGroup::ThisYear);
        assert_eq!(age_group("2024-01-20T12:00:00Z", now), AgeGroup::Older);
        let ended = |at: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            title: String::new(),
            author: String::new(),
            updated: at.into(),
            draft: false,
            ended: Some(Ended {
                how: Ending::Completed,
                at: at.into(),
            }),
        };
        let weeks = weekly(
            &[
                ended("2026-11-14T00:00:00Z"),
                ended("2026-11-10T00:00:00Z"),
                ended("2026-11-01T00:00:00Z"),
                ended("2025-01-01T00:00:00Z"),
            ],
            now,
            3,
        );
        assert_eq!(weeks, [0, 1, 2]);
    }

    #[test]
    fn a_drag_selects_across_lines_and_copies_what_was_drawn() {
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Kind::Pull,
            Load::Loading,
        );
        page.cache = Some((
            (80, Tab::Summary, false, 0, 0),
            Body {
                lines: vec![
                    Line::from(vec![Span::raw("hello "), Span::raw("world")]),
                    Line::from("second line"),
                    Line::from("third"),
                ],
                figures: Vec::new(),
            },
        ));
        // Dragged from "world" back up and to the start of "second": either way round reads the same.
        page.selection = Some(Selection {
            anchor: (1, 5),
            head: (0, 6),
        });
        assert_eq!(page.selected_text(), "world\nsecond");
        let lit = highlight(&Line::from("hello world"), 6, 11);
        assert_eq!(plain(&lit), "hello world");
        assert!(
            lit.spans
                .iter()
                .any(|s| s.content == "world" && s.style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn the_header_is_selectable_and_a_drag_runs_on_into_the_body() {
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Kind::Pull,
            Load::Loading,
        );
        page.header = vec![
            Line::default(),
            Line::from("fix: Find curly quotes"),
            Line::from("● Open   o/r#1 · marcos"),
        ];
        page.header_area = Rect::new(4, 0, 60, 3);
        page.body_area = Rect::new(4, 4, 60, 10);
        page.cache = Some((
            (60, Tab::Summary, false, 0, 0),
            Body {
                lines: vec![
                    Line::from("Status  Ready to merge"),
                    Line::from("Branch  fix/find"),
                ],
                figures: Vec::new(),
            },
        ));
        // A cell on the title row is a header point; one in the body comes after the header's lines.
        assert_eq!(page.point(9, 1), (1, 5));
        assert_eq!(page.point(12, 5), (4, 8));
        assert_eq!(page.body_line((4, 8)), Some(1));
        assert_eq!(page.body_line((1, 5)), None);
        page.selection = Some(Selection {
            anchor: page.point(9, 1),
            head: page.point(9, 4),
        });
        assert_eq!(
            page.selected_text(),
            "Find curly quotes\n● Open   o/r#1 · marcos\nStatus"
        );
    }

    #[test]
    fn a_drag_on_the_list_selects_a_rows_title_and_number() {
        let shown = vec![
            Line::default(),
            spread(
                vec![Span::raw("▌ "), Span::raw("●  "), Span::raw("Fix gifs")],
                vec![Span::raw("leaf #16")],
                30,
            ),
        ];
        let area = Rect::new(10, 5, 30, 10);
        // The title, then the number at the row's right end.
        let title = Selection {
            anchor: point_in(area, 0, 15, 6),
            head: point_in(area, 0, 22, 6),
        };
        assert_eq!(title.text(&shown), "Fix gifs");
        let number = Selection {
            anchor: point_in(area, 0, 37, 6),
            head: point_in(area, 0, 39, 6),
        };
        assert_eq!(number.text(&shown), "#16");
    }

    #[test]
    fn footer_keys_drop_from_the_end_to_fit() {
        assert_eq!(
            plain(&keys_line(&KEYS, 200)).matches("   ").count(),
            KEYS.len() - 1
        );
        assert_eq!(plain(&keys_line(&KEYS, 20)), "←→ tabs   m merge");
    }

    #[test]
    fn the_page_column_is_centered_and_capped() {
        assert_eq!(column(Rect::new(0, 0, 80, 24)), (3, 74));
        assert_eq!(column(Rect::new(0, 0, 200, 50)), (52, 96));
    }

    #[test]
    fn wrapping_keeps_words_whole_and_breaks_long_ones() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdef", 4), ["abcd", "ef"]);
        assert_eq!(wrap("a\nb", 10), ["a", "b"]);
    }

    #[test]
    fn spread_puts_the_right_side_flush_right() {
        let line = spread(vec![Span::raw("left")], vec![Span::raw("right")], 12);
        assert_eq!(plain(&line), "left   right");
    }
}
