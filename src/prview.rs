//! `leaf pr`: the open pull requests that concern you, and one read in full.
//!
//! The list shows the last run's answers at once and refreshes behind them;
//! the row under the cursor is read ahead, so opening it is instant. A pull
//! request has a Summary (status, branch, reviewers, checks that need you,
//! the description with its images and videos) and a Timeline (commits,
//! comments, reviews); Code hands the diff, fetched once per head commit, to
//! the reviewer; Checks lists every check by workflow, shows a failed job's
//! log, and runs failed jobs again. While checks run, the page reads itself
//! again. Text drags to select and is copied when let go. It can be merged,
//! closed, reopened, marked ready, approved and commented on.

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
    self, Action, CheckStatus, Detail, Entry, EventKind, Group, Mergeability, PrRef, Source, State,
    Verdict,
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
/// How often a pull request with running checks is read again.
const POLL: Duration = Duration::from_secs(20);
/// How often a running job's steps are read again while they're shown.
const JOB_POLL: Duration = Duration::from_secs(5);
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
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
    Viewed(PrRef, Box<Result<Detail, String>>),
    Acted(PrRef, Result<String, String>),
    /// The patch fetched for a head commit.
    Patched(PrRef, String, Result<String, String>),
    /// An Actions job's steps and log.
    Logged(u64, Result<pr::JobLog, String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Summary,
    Timeline,
    Checks,
}

/// What a click in the tab row asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabHit {
    Summary,
    Timeline,
    Code,
    Checks,
}

enum Mode {
    Normal,
    /// Picking how to merge.
    Merge,
    /// Picking which jobs to run again: each choice's key and action.
    Rerun(Vec<(char, Action)>),
    Confirm(Action),
    Compose {
        text: Vec<char>,
        cursor: usize,
    },
    Help,
}

enum Fetch {
    Loading,
    Done,
    Failed(String),
}

/// One heading of the list: what it found last, and whether it's asking again.
struct Slot {
    source: Source,
    entries: Option<Vec<Entry>>,
    fetch: Fetch,
}

struct List {
    slots: Vec<Slot>,
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
    copied: Option<String>,
    /// The rows' lines as drawn last, and where, so a drag can select their text.
    shown: Vec<Line<'static>>,
    shown_area: Rect,
    /// Text dragged over in `shown`.
    selection: Option<Selection>,
    dragging: bool,
}

impl List {
    fn entries(&self) -> Vec<(String, Vec<Entry>)> {
        let groups = pr::dedupe(
            self.slots
                .iter()
                .filter_map(|slot| {
                    Some(Group {
                        title: slot.source.title(),
                        entries: slot.entries.clone()?,
                    })
                })
                .collect(),
        );
        let filter = self.typed.to_lowercase();
        groups
            .into_iter()
            .map(|group| {
                let entries = group
                    .entries
                    .into_iter()
                    .filter(|e| {
                        filter.is_empty()
                            || e.title.to_lowercase().contains(&filter)
                            || e.pr.label().to_lowercase().contains(&filter)
                            || e.author.to_lowercase().contains(&filter)
                    })
                    .collect();
                (group.title, entries)
            })
            .collect()
    }

    fn picked(&self) -> Option<PrRef> {
        self.entries()
            .into_iter()
            .flat_map(|(_, list)| list)
            .nth(self.selected)
            .map(|e| e.pr)
    }

    fn count(&self) -> usize {
        self.entries().iter().map(|(_, e)| e.len()).sum()
    }

    fn loading(&self) -> bool {
        self.slots.iter().any(|s| matches!(s.fetch, Fetch::Loading))
    }

    fn failure(&self) -> Option<String> {
        self.slots.iter().find_map(|s| match &s.fetch {
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
    detail: Load<Detail>,
    tab: Tab,
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
    /// When the page last asked again for running checks.
    polled: Instant,
}

impl Page {
    fn new(pr: PrRef, detail: Load<Detail>) -> Self {
        Page {
            pr,
            detail,
            tab: Tab::Summary,
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
            self.follow = true;
        }
    }

    /// Shows the pull request read again, and returns the job to read when
    /// the open log's check now runs as another job.
    fn refreshed(&mut self, detail: Detail) -> Option<u64> {
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
        Start::Pr(pr) => app.open_page(pr),
    }
    let result = setup.and_then(|_| app.event_loop(&mut terminal));
    let _ = app.gallery.clear();
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

impl App {
    /// Shows the list from the last run's answers and asks every heading again, all at once.
    fn open_list(&mut self) {
        let slots = pr::sources(self.here.as_deref(), self.branch.as_deref())
            .into_iter()
            .enumerate()
            .map(|(index, source)| {
                let (tx, asked) = (self.tx.clone(), source.clone());
                std::thread::spawn(move || {
                    let result = pr::search(&asked).map_err(|e| e.to_string());
                    let _ = tx.send(Msg::Searched(index, result));
                });
                Slot {
                    entries: pr::cached_search(&source),
                    source,
                    fetch: Fetch::Loading,
                }
            })
            .collect();
        self.list = Some(List {
            slots,
            typed: String::new(),
            selected: 0,
            resting: None,
            hits: Vec::new(),
            pressed: None,
            copied: None,
            shown: Vec::new(),
            shown_area: Rect::default(),
            selection: None,
            dragging: false,
        });
    }

    /// Opens a pull request on what's known of it, and reads it again unless that's fresh.
    fn open_page(&mut self, pr: PrRef) {
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
            .or_else(|| pr::cached_view(&pr));
        let detail = match known {
            Some(detail) => {
                let head = detail.head_sha.clone();
                self.want_patch(&pr, &head);
                Load::Ready(detail)
            }
            None => Load::Loading,
        };
        self.page = Some(Page::new(pr.clone(), detail));
        self.fetch(pr, false);
    }

    fn fetch(&mut self, pr: PrRef, force: bool) {
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
            let result = pr::view(&pr).map_err(|e| e.to_string());
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
        let Some(picked) = list.picked() else { return };
        match &list.resting {
            Some((pr, at)) if *pr == picked => {
                if at.elapsed() >= SETTLE
                    && !self.details.contains_key(&picked)
                    && self.prefetched.insert(picked.clone())
                {
                    self.fetch(picked, false);
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
        let (tx, pr) = (self.tx.clone(), page.pr.clone());
        std::thread::spawn(move || {
            let result = pr::perform(&pr, &action).map_err(|e| e.to_string());
            let _ = tx.send(Msg::Acted(pr, result));
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

    /// Reads the shown pull request again every `POLL` while its checks run.
    fn poll_checks(&mut self) {
        let Some(page) = &mut self.page else { return };
        let Load::Ready(detail) = &page.detail else {
            return;
        };
        let running = pr::any_running(&detail.checks);
        let read = self
            .details
            .get(&page.pr)
            .is_none_or(|(_, at)| at.elapsed() >= POLL);
        let job = page.log.filter(|job| {
            matches!(self.logs.jobs.get(job), Some(Load::Ready(log)) if !log.done)
                && page.log_polled.elapsed() >= JOB_POLL
        });
        if running && read && page.polled.elapsed() >= POLL {
            page.polled = Instant::now();
            let pr = page.pr.clone();
            self.fetch(pr, true);
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
                if let Some(slot) = self.list.as_mut().and_then(|l| l.slots.get_mut(index)) {
                    match result {
                        Ok(entries) => {
                            slot.entries = Some(entries);
                            slot.fetch = Fetch::Done;
                        }
                        Err(e) => slot.fetch = Fetch::Failed(e),
                    }
                }
            }
            Msg::Viewed(pr, result) => {
                self.inflight.remove(&pr);
                let shown = self.page.as_ref().is_some_and(|p| p.pr == pr);
                match *result {
                    Ok(detail) => {
                        if shown {
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
            Msg::Acted(pr, result) => {
                if let Some(page) = self.page.as_mut().filter(|p| p.pr == pr) {
                    page.busy = None;
                    page.toast = Some(match result {
                        Ok(done) => done,
                        Err(e) => e,
                    });
                }
                self.fetch(pr, true);
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
                    || (page.tab == Tab::Summary && self.gallery.loading())
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
            // A video plays only while its description is on screen.
            if self.page.as_ref().is_none_or(|p| p.tab != Tab::Summary) {
                self.gallery.stop();
            }
            if let Some(failed) = self.gallery.tick()
                && let Some(page) = &mut self.page
            {
                page.toast = Some(failed);
            }
            self.prefetch();
            self.poll_checks();
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

    fn list_mouse(&mut self, mouse: MouseEvent) {
        let Some(list) = &mut self.list else { return };
        let last = list.count().saturating_sub(1);
        let point = point_in(list.shown_area, 0, mouse.column, mouse.row);
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
                                list.copied =
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
                    self.open_page(pr);
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
        list.copied = None;
        list.selection = None;
        match key.code {
            // Letters filter the list, so copying the number takes ctrl.
            KeyCode::Char('y') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(pr) = &picked {
                    let number = format!("#{}", pr.number);
                    copy(&number);
                    list.copied = Some(format!("Copied {number}"));
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
                    self.open_page(pr);
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
            Mode::Compose { text, cursor } => {
                let alt = key.modifiers.contains(KeyModifiers::ALT);
                match key.code {
                    KeyCode::Esc => page.mode = Mode::Normal,
                    KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                        text.insert(*cursor, '\n');
                        *cursor += 1;
                    }
                    KeyCode::Enter => {
                        let body: String = text.iter().collect::<String>().trim().to_owned();
                        if body.is_empty() {
                            page.mode = Mode::Normal;
                        } else {
                            self.act(Action::Comment(body));
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
            KeyCode::Char('q') | KeyCode::Esc => {
                self.page = None;
                if self.list.is_none() {
                    return Ok(false);
                }
            }
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
            // Tab cycles Summary, Timeline, Code, Checks; shift-tab goes the other way.
            KeyCode::Tab => match page.tab {
                Tab::Summary => page.show(Tab::Timeline),
                Tab::Timeline => self.open_code(terminal)?,
                Tab::Checks => page.show(Tab::Summary),
            },
            KeyCode::BackTab => match page.tab {
                Tab::Summary => page.show(Tab::Checks),
                Tab::Timeline => page.show(Tab::Summary),
                Tab::Checks => self.open_code(terminal)?,
            },
            KeyCode::Char('1') => page.show(Tab::Summary),
            KeyCode::Char('2') => page.show(Tab::Timeline),
            KeyCode::Char('3') => self.open_code(terminal)?,
            KeyCode::Char('4') => page.show(Tab::Checks),
            KeyCode::Left | KeyCode::Char('h') => match page.tab {
                Tab::Summary | Tab::Timeline => page.show(Tab::Summary),
                Tab::Checks => self.open_code(terminal)?,
            },
            KeyCode::Right | KeyCode::Char('l') => match page.tab {
                Tab::Summary => page.show(Tab::Timeline),
                Tab::Timeline => self.open_code(terminal)?,
                Tab::Checks => {}
            },
            KeyCode::Char('e') => {
                page.expanded = !page.expanded;
                page.cache = None;
            }
            KeyCode::Char('?') => page.mode = Mode::Help,
            KeyCode::Char('R') => {
                let pr = page.pr.clone();
                self.fetch(pr, true);
            }
            KeyCode::Char('o') => {
                page.toast = Some(match pr::open_in_browser(&page.pr) {
                    Ok(()) => format!("Opened {} in the browser", page.pr.label()),
                    Err(e) => e.to_string(),
                });
            }
            KeyCode::Char('y') => {
                copy(&page.pr.checkout());
                page.toast = Some(format!("Copied {}", page.pr.checkout()));
            }
            KeyCode::Char('Y') => {
                copy(&page.pr.url());
                page.toast = Some(format!("Copied {}", page.pr.url()));
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
    Confirm(Action),
    Now(Action),
}

fn available(detail: &Detail, key: char) -> Result<Next, String> {
    let open = matches!(detail.state, State::Open | State::Draft);
    let state = || {
        format!(
            "#{} is {}",
            detail.pr.number,
            detail.state.label().to_lowercase()
        )
    };
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
            State::Closed => Ok(Next::Confirm(Action::Reopen)),
            State::Open | State::Draft => Ok(Next::Confirm(Action::Close)),
        },
        'r' => match detail.state {
            State::Draft => Ok(Next::Now(Action::Ready)),
            State::Open => Ok(Next::Confirm(Action::Draft)),
            State::Merged | State::Closed => Err(state()),
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
        State::Merged => PURPLE,
        State::Closed => RED,
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

/// The list: a title, a filter, then each heading's pull requests on two
/// lines, title above and who and when below, in one centered column.
fn draw_list(f: &mut Frame, list: &mut List, now: i64, ink: Option<art::Ink>) {
    let area = f.area();
    let (x, width) = column(area);
    let w = width as usize;
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let entries = list.entries();
    let total: usize = entries.iter().map(|(_, e)| e.len()).sum();
    list.selected = list.selected.min(total.saturating_sub(1));

    // The title row, and how many there are or that they're being asked for.
    let status = if list.loading() {
        format!("{}  updating", spinner())
    } else if total > 0 {
        format!("{total} open")
    } else {
        String::new()
    };
    f.render_widget(
        Paragraph::new(spread(
            vec![Span::styled("Pull requests", bold)],
            vec![Span::styled(status, dim)],
            w,
        )),
        Rect::new(x, area.y + 1, width, 1),
    );
    let field = if list.typed.is_empty() {
        Line::from(vec![
            Span::styled("/  ", dim),
            Span::styled("▏", bold),
            Span::styled("Filter by title, repository, number or author", dim),
        ])
    } else {
        Line::from(vec![
            Span::styled("/  ", dim),
            Span::raw(list.typed.clone()),
            Span::styled("▏", bold),
        ])
    };
    f.render_widget(Paragraph::new(field), Rect::new(x, area.y + 3, width, 1));

    let top = area.y + 5;
    let bottom = (area.y + area.height).saturating_sub(2);
    let room = bottom.saturating_sub(top) as usize;

    if total == 0 {
        list.shown.clear();
        list.selection = None;
        let text = if list.loading() {
            format!("{}  Loading your pull requests", spinner())
        } else if let Some(e) = list.failure().filter(|_| list.typed.is_empty()) {
            e
        } else if list.typed.is_empty() {
            "No open pull requests concern you.".to_owned()
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
        let mut index = 0;
        for (title, group) in entries.iter().filter(|(_, e)| !e.is_empty()) {
            if !lines.is_empty() {
                lines.push((Line::default(), None));
            }
            lines.push((
                Line::from(vec![
                    Span::styled(title.to_uppercase(), dim.add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {}", group.len()), dim),
                ]),
                None,
            ));
            lines.push((Line::default(), None));
            for entry in group {
                let picked = index == list.selected;
                let bar = if picked { "▌ " } else { "  " };
                let (glyph, color) = if entry.draft {
                    ("○", DIM)
                } else {
                    ("●", GREEN)
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
                let mut about = format!("{} · {}", entry.author, pr::ago(&entry.updated, now));
                if entry.draft {
                    about.push_str(" · draft");
                }
                lines.push((
                    Line::from(vec![Span::raw("     "), Span::styled(about, dim)]),
                    Some(index),
                ));
                index += 1;
            }
        }
        // Scroll so the picked row and its heading stay in view.
        let first_of = |wanted: usize| lines.iter().position(|(_, i)| *i == Some(wanted));
        let at = first_of(list.selected).unwrap_or(0);
        let skip = (at + 2).saturating_sub(room).min(at.saturating_sub(2));
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

    let footer = match (list.failure(), &list.copied) {
        (_, Some(copied)) => Line::from(Span::styled(clip(copied, w), dim)),
        (Some(e), None) if total > 0 => Line::from(Span::styled(clip(&e, w), dim)),
        _ => keys_line(
            &[
                ("↑↓", "move"),
                ("enter", "open"),
                ("type", "filter"),
                ("^y", "copy #"),
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
    header.push(spread(
        meta,
        vec![
            Span::styled(format!("+{}", detail.additions), Style::default().fg(GREEN)),
            Span::raw(" "),
            Span::styled(format!("−{}", detail.deletions), Style::default().fg(RED)),
        ],
        w,
    ));
    header.push(Line::default());
    let tabs_row = area.y + header.len() as u16;
    let mut tabs = Vec::new();
    page.tab_hits.clear();
    let mut at = x;
    for (label, hit, on) in [
        ("Summary", TabHit::Summary, page.tab == Tab::Summary),
        ("Timeline", TabHit::Timeline, page.tab == Tab::Timeline),
        ("Code", TabHit::Code, false),
        ("Checks", TabHit::Checks, page.tab == Tab::Checks),
    ] {
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
            Tab::Timeline => Body {
                lines: timeline(detail, w, now, page.expanded),
                figures: Vec::new(),
            },
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
        draw_help(f, area, ink);
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
        Mode::Compose { text, cursor } => {
            let height = footer_height(page, width);
            let top = y + 1 - height;
            let mut lines = vec![
                Line::default(),
                Line::from(vec![
                    Span::styled(format!("Comment on #{}", page.pr.number), accent),
                    Span::styled("   enter send · alt+enter new line · esc cancel", dim),
                ]),
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
        Mode::Confirm(action) => {
            let question = match action {
                Action::Merge(method) => {
                    format!("{} #{} into {base}?", method.label(), page.pr.number)
                }
                Action::Close => format!("Close #{}?", page.pr.number),
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
            _ if page.tab == Tab::Checks && page.log.is_some() => {
                let mut keys = LOG_KEYS;
                if page.expanded {
                    keys[1].1 = "close all groups";
                }
                keys_line(&keys, width as usize)
            }
            _ if page.tab == Tab::Checks => keys_line(&CHECK_KEYS, width as usize),
            _ => keys_line(&KEYS, width as usize),
        },
    };
    f.render_widget(Paragraph::new(line), Rect::new(x, y, width, 1));
}

fn draw_help(f: &mut Frame, area: Rect, ink: Option<art::Ink>) {
    let keys = [
        ("←→, tab", "Summary, Timeline, Code, Checks"),
        ("1, 2, 3, 4", "the same tabs"),
        ("↑↓, enter", "Checks: a job's steps and log"),
        ("j/k, ctrl+d/u", "scroll a line, half a screen"),
        ("space/b, g/G", "a screen, top and bottom"),
        ("drag", "select text; it's copied"),
        ("click", "a picture opens it, a video plays"),
        ("e", "long comments, or a job's groups"),
        ("m", "merge: squash, merge commit or rebase"),
        ("x", "close, or reopen a closed one"),
        ("r", "ready or draft; on Checks, re-run"),
        ("a", "approve"),
        ("c", "comment"),
        ("o", "open on GitHub, or the picked check"),
        ("y, Y, n", "copy the checkout command, link, number"),
        ("click #", "the number in the header copies it"),
        ("R", "reload"),
        ("q, esc", "back to the list, or quit"),
    ];
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

    let (status, color) = match (detail.state, detail.mergeability) {
        (State::Merged, _) => ("Merged".to_owned(), PURPLE),
        (State::Closed, _) => ("Closed without merging".to_owned(), RED),
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
            lines.push(Line::from(Span::styled(format!("      {note}"), dim)));
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
    let prepared = prmedia::prepare(&body);
    gallery.learn(&prepared);
    let laid = {
        let gallery = &*gallery;
        render::layout(&prepared.markdown, width.max(10), gallery.cell(), &|href| {
            gallery.size(href)
        })
    };
    for figure in &laid.figures {
        gallery.want(&figure.href);
    }
    let mut lines = rows(&laid.rows, 0);
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

fn timeline(detail: &Detail, width: usize, now: i64, expanded: bool) -> Vec<Line<'static>> {
    let dim = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    let indent = 4u16;
    for event in &detail.timeline {
        let when = Span::styled(pr::ago(&event.at, now), dim);
        let actor = Span::styled(event.actor.clone(), bold);
        let (glyph, color, what, body): (&str, Color, Vec<Span<'static>>, Option<String>) =
            match &event.kind {
                EventKind::Opened => (
                    "●",
                    GREEN,
                    vec![actor, Span::styled(" opened this pull request", dim)],
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
                    RED,
                    vec![Span::styled(
                        "Closed",
                        Style::default().fg(RED).add_modifier(Modifier::BOLD),
                    )],
                    None,
                ),
            };
        let mut left = vec![Span::styled(
            format!("{glyph}  "),
            Style::default().fg(color),
        )];
        left.extend(what);
        lines.push(spread(left, vec![when], width));
        if let Some(body) = body.map(|b| pr::visible(&b)).filter(|b| !b.is_empty()) {
            let rendered = markdown(&body, (width as u16).saturating_sub(indent), indent);
            let total = rendered.len();
            let shown = if expanded { total } else { total.min(EXCERPT) };
            lines.extend(rendered.into_iter().take(shown).map(|mut line| {
                line.spans.insert(0, Span::styled("│", dim));
                line.spans[1] = Span::styled(
                    line.spans[1].content.chars().skip(1).collect::<String>(),
                    line.spans[1].style,
                );
                line
            }));
            if shown < total {
                lines.push(Line::from(Span::styled(
                    format!("│   … {} more lines · e expands", total - shown),
                    dim,
                )));
            }
        }
        lines.push(Line::from(Span::styled("│", dim)));
    }
    lines.pop();
    lines
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
        let job = Load::Ready(pr::JobLog {
            steps,
            done: true,
            missing: None,
        });
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
            Load::Loading,
        );
        page.open_steps.insert(0);
        for width in [12, 16, 24, 40] {
            let view = job_view(None, Some(&job), &page, width, 0);
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
                    at: "2026-11-12T03:00:00Z".into(),
                    actor: "marcos".into(),
                    kind: EventKind::Opened,
                },
                pr::Event {
                    at: "2026-11-12T03:30:00Z".into(),
                    actor: "bot".into(),
                    kind: EventKind::Comment {
                        body: (1..=30).map(|n| format!("line {n}\n\n")).collect(),
                    },
                },
            ],
        }
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| plain(l).trim_end().to_owned())
            .collect()
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
        let short = texts(&timeline(&detail(State::Open), 60, now, false));
        assert!(short[0].starts_with("●  marcos opened this pull request"));
        assert!(short.iter().any(|l| l.contains("more lines · e expands")));
        let long = texts(&timeline(&detail(State::Open), 60, now, true));
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
    fn the_list_shows_each_pull_request_once_and_filters() {
        let entry = |number, title: &str| Entry {
            pr: PrRef {
                repo: "o/r".into(),
                number,
            },
            title: title.into(),
            author: "me".into(),
            updated: "2026-11-12T04:00:00Z".into(),
            draft: false,
        };
        let mut list = List {
            slots: vec![
                Slot {
                    source: Source::Yours,
                    entries: Some(vec![entry(1, "fix quotes"), entry(2, "add pr")]),
                    fetch: Fetch::Done,
                },
                Slot {
                    source: Source::Reviewing,
                    entries: Some(vec![entry(2, "add pr")]),
                    fetch: Fetch::Loading,
                },
                Slot {
                    source: Source::Owned,
                    entries: None,
                    fetch: Fetch::Loading,
                },
            ],
            typed: String::new(),
            selected: 0,
            resting: None,
            hits: Vec::new(),
            pressed: None,
            copied: None,
            shown: Vec::new(),
            shown_area: Rect::default(),
            selection: None,
            dragging: false,
        };
        assert_eq!(list.count(), 2);
        assert!(list.loading());
        list.typed = "quotes".into();
        assert_eq!(list.picked().map(|pr| pr.number), Some(1));
    }

    #[test]
    fn a_drag_selects_across_lines_and_copies_what_was_drawn() {
        let mut page = Page::new(
            PrRef {
                repo: "o/r".into(),
                number: 1,
            },
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
