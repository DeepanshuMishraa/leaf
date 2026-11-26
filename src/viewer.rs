//! The reader: pages on screen, keys, mouse selection, highlights and figures.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use dither_ratatui::dither_core::Field;
use image::{DynamicImage, RgbaImage};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use unicode_width::UnicodeWidthChar;

use crate::agent::{self, Agent, AgentFailed};
use crate::aloud::{self, Backend, Cut, Narrator, Said, Spot, Status};
use crate::art;
use crate::article;
use crate::doc::{Doc, ImageRef, WATCH_TICK, Watch};
use crate::edge;
use crate::find::Query;
use crate::follow::{self, Follow};
use crate::highlight::{self, Highlight};
use crate::hue::Hue;
use crate::kitty::{self, Kitty};
use crate::look::Look;
use crate::mono::{self, Rgb};
use crate::news::News;
use crate::picture;
use crate::progress::{Bookmark, ImageMode, Progress, Scroll};
use crate::remote::{self, Command};
use crate::render::{self, DIM, Laid};
use crate::stroke::{self, Cells, Corner, FRAME_PAD, Point, Stroke, Trace};
use crate::usage;

const SPEECH_SENTENCE_REWIND: usize = 76;
/// Windows at least this big show one page per screen; smaller ones scroll.
const PAGED_MIN_WIDTH: u16 = 100;
/// Columns `<` and `>` take from or add to the reading column.
const WIDTH_STEP: u16 = 8;
/// The narrowest column `<` goes to.
const MIN_WIDTH: u16 = 20;

/// How wide the reading column is asked to be, before the screen limits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadingWidth {
    Columns(u16),
    /// As wide as the screen allows.
    Fit,
}

impl ReadingWidth {
    /// The column width this asks for on a screen with `room` columns to spare.
    fn within(self, room: u16) -> u16 {
        match self {
            ReadingWidth::Columns(columns) => columns.min(room),
            ReadingWidth::Fit => room,
        }
        .max(1)
    }
}
const PAGED_MIN_HEIGHT: u16 = 24;
/// The footer line with a blank row above and below it.
const FOOTER_ROWS: u16 = 3;
/// Rows one turn of the mouse wheel scrolls.
const WHEEL_STEP: isize = 3;
/// Ink for painted images when the terminal doesn't report its text color.
const FALLBACK_INK: Rgb = [220, 220, 220];
/// Paper under diagrams when the terminal doesn't report its background.
const FALLBACK_PAPER: Rgb = [30, 30, 30];
/// Laid-out pages kept on each side of the one being read.
const KEPT_PAGES: usize = 4;
/// How much of a page's first line names its bookmark.
const BOOKMARK_LABEL_CHARS: usize = 48;
/// How much text to look for when an edited page is reopened from history.
const HISTORY_ANCHOR_CHARS: usize = 48;
/// How long a search reads pages before the screen is drawn and keys are read again.
const SEARCH_SLICE: Duration = Duration::from_millis(30);
/// How often the word read aloud is checked: fast enough that the mark
/// keeps up with the voice at 4×.
const NARRATION_TICK: Duration = Duration::from_millis(15);

/// Ink is drawn over figures.
const INK_LAYER: i32 = 1;
/// How close, in cells, the eraser has to pass to a stroke to take it.
const ERASER_REACH: f32 = 0.7;
/// How close, in cells, a click has to be to a stroke to pick it up.
const PICK_REACH: f32 = 1.0;
/// A picked stroke's frame is drawn over all ink.
const FRAME_LAYER: i32 = 2;
/// How much `+` grows a picked stroke, and `-` shrinks it.
const GROW: f32 = 1.15;

const SELECTION: Color = Color::Indexed(8);
// Dark ink on the light marker colors reads the same on light and dark themes.
const MARK_INK: Color = Color::Rgb(20, 20, 20);
/// Search matches in light orange, the one the reader is on in a deeper orange.
const FOUND: Color = Color::Rgb(255, 196, 140);
const FOUND_HERE: Color = Color::Rgb(255, 128, 48);

/// `Paged` shows one source page per screen; `Scroll` stacks every page in
/// one column with the page number between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Paged,
    Scroll,
}

/// A place in a document, with a text anchor in case the page changes or reflows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Viewport {
    page: usize,
    offset: usize,
    anchor: Option<HistoryAnchor>,
    width: u16,
    mode: Mode,
    look: Option<crate::doc::Look>,
}

#[derive(Debug, PartialEq, Eq)]
struct HistoryAnchor {
    offset: usize,
    excerpt: Vec<char>,
    page_hash: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ReadingLocation {
    path: PathBuf,
    viewport: Viewport,
}

/// Places reached through reader navigation in the current open session.
#[derive(Debug, Default)]
pub(crate) struct ReaderHistory {
    back: VecDeque<ReadingLocation>,
    forward: VecDeque<ReadingLocation>,
}

impl ReaderHistory {
    const LIMIT: usize = 100;

    fn push(stack: &mut VecDeque<ReadingLocation>, location: ReadingLocation) {
        if stack.back() == Some(&location) {
            return;
        }
        if stack.len() == Self::LIMIT {
            stack.pop_front();
        }
        stack.push_back(location);
    }
}

/// What floats over the text: nothing, the key list, the go-to-page box,
/// the find box with what's typed so far, the question for an agent, or the
/// bookmark list with the one picked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dialog {
    None,
    Help,
    /// How to highlight, pick colors and draw.
    Guide,
    Goto {
        digits: String,
        page_count: usize,
    },
    Find {
        typed: String,
    },
    /// A question about `passage`, the selection copied when the box opened.
    Ask {
        typed: String,
        passage: String,
    },
    Bookmarks {
        selected: usize,
    },
    /// The voices to read aloud with, filtered by what's typed.
    /// `selected` is the row moved to; until then, the voice reading now.
    Voices {
        typed: String,
        selected: Option<usize>,
    },
}

/// Which way to move through pages or history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

impl Direction {
    /// The page looked at `step` pages on from `from`, wrapping round the end.
    fn page(self, from: usize, step: usize, pages: usize) -> usize {
        match self {
            Direction::Forward => (from + step) % pages,
            Direction::Backward => (from + pages - step % pages) % pages,
        }
    }
}

/// Where the query was found: characters `start..end` of a page's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match {
    page: usize,
    start: usize,
    end: usize,
}

/// How far a search has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seek {
    /// Reading pages for the next match. Matches on `from`'s page count from
    /// its offset on (or, going back, before it); `step` pages have been read.
    /// The search wraps round, ending on the part of `from`'s page it skipped.
    Seeking {
        direction: Direction,
        from: TextPos,
        step: usize,
    },
    Found(Match),
    Missing,
}

struct Search {
    query: Query,
    seek: Seek,
}

/// Finds visible text and reports matches in the original page offsets.
fn page_matches(query: &Query, laid: &Laid) -> Vec<(usize, usize)> {
    if laid.hidden_link_urls.is_empty() {
        return query.matches(&laid.text);
    }

    let mut offsets = Vec::with_capacity(laid.text.len());
    let mut start = 0;
    for hidden in &laid.hidden_link_urls {
        offsets.extend(start..hidden.start);
        start = hidden.end;
    }
    offsets.extend(start..laid.text.len());
    let text: Vec<_> = offsets.iter().map(|&offset| laid.text[offset]).collect();
    query
        .matches(&text)
        .into_iter()
        .map(|(start, end)| (offsets[start], offsets[end - 1] + 1))
        .collect()
}

/// A place in a page's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPos {
    page: usize,
    offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: TextPos,
    head: TextPos,
}

impl Selection {
    fn range(self) -> (TextPos, TextPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// What dragging the mouse does: select text, draw, rub strokes out, or
/// pick a stroke up to move and resize it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Select,
    Pen,
    Eraser,
    Move,
}

/// A stroke, and the serial its drawn images are kept under.
#[derive(Debug, Clone, PartialEq)]
struct Inked {
    serial: u64,
    stroke: Stroke,
}

/// What the mouse button held down is doing.
#[derive(Debug, Clone, PartialEq)]
enum Drag {
    Idle,
    Selecting,
    Inking(Inked),
    /// `erased` once a stroke has gone, so letting go saves.
    Erasing {
        erased: bool,
    },
    /// Carrying the picked stroke: `from` is the cell the mouse took hold
    /// at, `original` the stroke as it was then.
    Moving {
        from: (u16, u16),
        original: Stroke,
    },
    /// Stretching the picked stroke by one corner of its frame.
    Resizing {
        corner: Corner,
        original: Stroke,
    },
}

/// A stroke's image as drawn: which stroke, how far it had got, the cells
/// of it on screen, measured from its anchor, and whether it's the stroke
/// or the frame round it when picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct InkKey {
    serial: u64,
    points: usize,
    clip: Cells,
    frame: bool,
}

enum Remote {
    Loading,
    Ready(Rc<RgbaImage>),
    Failed,
}

/// A figure slice ready to draw.
enum Drawn {
    Kitty(kitty::Image),
    Protocol(Protocol),
}

/// A figure drawn for one image mode and one visible slice of its rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProtocolKey {
    href: String,
    mode: ImageMode,
    cols: u16,
    from: u16,
    to: u16,
}

/// What is marked on a row besides its own styling: the selection, the
/// search's matches on its page, and the match the reader is on.
#[derive(Clone, Copy)]
struct Marks<'a> {
    selection: Option<(TextPos, TextPos)>,
    found: &'a [(usize, usize)],
    here: Option<Match>,
    /// The word being read aloud.
    said: Option<Said>,
}

/// What a row on screen shows.
#[derive(Debug, Clone, Copy)]
struct ScreenRow {
    page: usize,
    row: usize,
}

/// Hears the full set of highlights after each change.
pub type HighlightsListener = Box<dyn FnMut(&[Highlight])>;

/// Opens a web link outside leaf.
type UrlOpener = Box<dyn Fn(&str) -> io::Result<()>>;
pub type BookmarksListener = Box<dyn FnMut(&[Bookmark])>;
pub type StrokesListener = Box<dyn FnMut(&[Stroke])>;

pub struct ViewerOptions {
    pub max_text_width: u16,
    pub colorful_markdown: bool,
    pub hide_link_urls: bool,
    /// For paged documents, where reading resumes (1-based).
    pub start_page: Option<usize>,
    pub image_mode: ImageMode,
    pub highlights: Vec<Highlight>,
    /// Hears each page the reader settles on (1-based).
    pub on_page: Box<dyn FnMut(Progress)>,
    /// For documents without pages, where reading resumes: a place in the page text.
    pub start_anchor: Option<usize>,
    /// A session history position takes precedence over saved reading progress.
    pub(crate) start_viewport: Option<Viewport>,
    pub(crate) start_notice: Option<String>,
    /// Hears the place in a document without pages as each percent goes by, and on quitting.
    pub on_scroll: Box<dyn FnMut(Scroll)>,
    pub on_image_mode: Box<dyn FnMut(ImageMode)>,
    /// Hears every highlight there is, each time one is added or cleared.
    pub on_highlights: HighlightsListener,
    /// For paged documents, the pages marked to come back to, in page order.
    pub bookmarks: Vec<Bookmark>,
    /// Hears every bookmark there is, each time one is added or removed.
    pub on_bookmarks: BookmarksListener,
    /// Ink drawn on the document, oldest first.
    pub strokes: Vec<Stroke>,
    /// Hears every stroke there is, each time one is drawn or erased.
    pub on_strokes: StrokesListener,
    /// The voice that reads aloud, by its service name.
    pub voice: String,
    /// How fast it reads, 1.0 being normal.
    pub speed: f32,
    pub on_voice: Box<dyn FnMut(&str)>,
    pub on_speed: Box<dyn FnMut(f32)>,
    /// Where speech comes from and goes.
    pub speech: Backend,
    /// Path or URL this document was opened from, when leaf knows it.
    pub source: Option<String>,
    /// Original filesystem path, which may differ from the progress key.
    pub picture_path: Option<PathBuf>,
    pub watch: Option<Watch>,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        ViewerOptions {
            max_text_width: crate::config::DEFAULT_MAX_WIDTH,
            colorful_markdown: false,
            hide_link_urls: false,
            start_page: None,
            image_mode: ImageMode::Color,
            highlights: Vec::new(),
            on_page: Box::new(|_| {}),
            start_anchor: None,
            start_viewport: None,
            start_notice: None,
            on_scroll: Box::new(|_| {}),
            on_image_mode: Box::new(|_| {}),
            on_highlights: Box::new(|_| {}),
            bookmarks: Vec::new(),
            on_bookmarks: Box::new(|_| {}),
            strokes: Vec::new(),
            on_strokes: Box::new(|_| {}),
            voice: edge::DEFAULT_VOICE.to_owned(),
            speed: 1.0,
            on_voice: Box::new(|_| {}),
            on_speed: Box::new(|_| {}),
            speech: Backend::Edge,
            source: None,
            picture_path: None,
            watch: None,
        }
    }
}

pub struct App {
    doc: Doc,
    options: ViewerOptions,
    picker: Picker,
    /// Column width pages are laid out at.
    width: u16,
    /// Starts at the configured width; `<`, `>` and `=` change it.
    reading_width: ReadingWidth,
    /// Columns the last frame left for the reading column; unbounded before it.
    room: u16,
    laid: HashMap<usize, (Rc<Laid>, bool)>,
    mode: Mode,
    /// The page being read: in paged mode the one shown, when scrolling the one at the top.
    current: usize,
    /// Rows of the current page scrolled past.
    offset: usize,
    /// Rows the text area shows.
    view_height: u16,
    image_mode: ImageMode,
    /// How an office document is shown (`o` switches); `None` for other documents.
    shown_as: Option<crate::doc::Look>,
    ink: Option<Rgb>,
    paper: Option<Rgb>,
    /// The theme and the UI setting, followed while the document is open.
    look: Look,
    /// This version's notes, up once after an update until OK.
    news: Option<News>,
    /// Time spent reading, for the home screen's heat map.
    clock: usage::Clock,
    highlights: Vec<Highlight>,
    /// The color new highlights and ink come in.
    hue: Hue,
    /// A word in the footer about the last key, gone at the next one.
    notice: Option<String>,
    /// Where a question about a selection is sent. Tests stand in for the
    /// agent pane; otherwise leaf finds one in this Rex session.
    ask: Option<Box<dyn FnMut(&str) -> Result<Agent, AgentFailed>>>,
    /// Opens a web link from the page. Tests stand in for the browser.
    open_url: UrlOpener,
    /// A command from Rex's command palette, being typed in after F12.
    rex_command: Option<String>,
    bookmarks: Vec<Bookmark>,
    selection: Option<Selection>,
    tool: Tool,
    drag: Drag,
    strokes: Vec<Inked>,
    /// The serial the next stroke gets.
    next_serial: u64,
    /// Strokes' images the terminal holds, for the parts of them on screen.
    ink_drawn: HashMap<InkKey, kitty::Image>,
    /// The serial of the stroke picked up with the move tool.
    picked: Option<u64>,
    dialog: Dialog,
    remote: HashMap<String, Remote>,
    fetched_tx: mpsc::Sender<(String, Option<RgbaImage>)>,
    fetched_rx: mpsc::Receiver<(String, Option<RgbaImage>)>,
    /// Figures' pixels while on screen; `None` for one that can't be decoded, so it isn't tried again each draw.
    pixels: HashMap<(String, ImageMode), Option<Rc<RgbaImage>>>,
    protocols: HashMap<ProtocolKey, Drawn>,
    /// Set when kitty images are placed directly instead of through ratatui-image.
    kitty: Option<Kitty>,
    /// The kitty images the last draw wants on screen.
    placements: Vec<(kitty::Image, Rect)>,
    /// The text column on screen, and what each of its rows shows, as last drawn.
    column: Rect,
    screen: Vec<Option<ScreenRow>>,
    reported: Option<Progress>,
    /// A source offset survives reflow; a wrapped row number does not.
    anchor: Option<usize>,
    reported_scroll: Option<Scroll>,
    copy: Option<String>,
    search: Option<Search>,
    narrator: Narrator,
    /// Whether the screen keeps the word being read aloud in view. Scrolling
    /// away lets it go; it picks up again once the word is back on screen.
    follow: bool,
    watch: Option<Watch>,
    /// For a picture file with others beside it: its place among them and how many there are.
    folder: Option<(usize, usize)>,
    /// A request to open another document, once the reader has closed.
    go: Option<Go>,
    quit: bool,
}

/// The row picked in the voice list: the one moved to, or else the voice
/// reading now when nothing's typed, or the best match when something is.
fn picked_voice(
    ranked: &[&edge::Voice],
    current: &str,
    typed: &str,
    selected: Option<usize>,
) -> usize {
    selected
        .unwrap_or_else(|| match typed {
            "" => ranked.iter().position(|v| v.name == current).unwrap_or(0),
            _ => 0,
        })
        .min(ranked.len().saturating_sub(1))
}

/// The character a key types.
fn typed(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) => Some(c),
        _ => None,
    }
}

fn char_width(c: char) -> u16 {
    c.width().unwrap_or(0) as u16
}

/// Where the ink's corner goes when a frame's `corner` handle is dragged to
/// `p`: the frame stands off the ink, so the handle sits that far outside it.
fn handle_to_ink(corner: Corner, p: Point) -> Point {
    let (sx, sy) = match corner {
        Corner::TopLeft => (1.0, 1.0),
        Corner::TopRight => (-1.0, 1.0),
        Corner::BottomRight => (-1.0, -1.0),
        Corner::BottomLeft => (1.0, -1.0),
    };
    Point {
        x: p.x + sx * FRAME_PAD.x,
        y: p.y + sy * FRAME_PAD.y,
    }
}

impl App {
    pub fn new(doc: Doc, mut options: ViewerOptions, picker: Picker) -> Self {
        let current = match (doc.page_count, options.start_page) {
            (Some(count), Some(start)) => start.clamp(1, count.max(1)) - 1,
            _ => 0,
        };
        let (doc_paged, options_anchor) = (doc.page_count.is_some(), options.start_anchor);
        let (fetched_tx, fetched_rx) = mpsc::channel();
        let kitty = (picker.protocol_type() == ProtocolType::Kitty && !picker.tmux_detected())
            .then(Kitty::default);
        let strokes: Vec<Inked> = options
            .strokes
            .iter()
            .zip(0..)
            .map(|(stroke, serial)| Inked {
                serial,
                stroke: stroke.clone(),
            })
            .collect();
        let watch = options.watch.take();
        let notice = options.start_notice.take();
        if let Some(look) = options
            .start_viewport
            .as_ref()
            .and_then(|viewport| viewport.look)
        {
            doc.source.set_look(look);
        }
        let folder = Self::folder_place(&doc, options.picture_path.as_deref());
        let narrator = Narrator::new(
            options.voice.clone(),
            options.speed,
            std::mem::replace(&mut options.speech, Backend::Edge),
        );
        App {
            narrator,
            follow: true,
            next_serial: strokes.len() as u64,
            strokes,
            tool: Tool::Select,
            drag: Drag::Idle,
            picked: None,
            ink_drawn: HashMap::new(),
            image_mode: options.image_mode,
            shown_as: doc.source.look(),
            highlights: options.highlights.clone(),
            hue: Hue::default(),
            notice,
            ask: None,
            open_url: Box::new(crate::prmedia::open),
            rex_command: None,
            bookmarks: options.bookmarks.clone(),
            reading_width: ReadingWidth::Columns(options.max_text_width),
            room: u16::MAX,
            doc,
            options,
            picker,
            width: 0,
            laid: HashMap::new(),
            mode: Mode::Scroll,
            current,
            offset: 0,
            view_height: 1,
            ink: None,
            paper: None,
            look: Look::detect(),
            news: News::unseen(),
            clock: usage::Clock::start(),
            selection: None,
            dialog: Dialog::None,
            remote: HashMap::new(),
            fetched_tx,
            fetched_rx,
            pixels: HashMap::new(),
            protocols: HashMap::new(),
            kitty,
            placements: Vec::new(),
            column: Rect::default(),
            screen: Vec::new(),
            reported: None,
            anchor: if doc_paged { None } else { options_anchor },
            reported_scroll: None,
            copy: None,
            search: None,
            watch,
            folder,
            go: None,
            quit: false,
        }
    }

    /// Where a picture file sits among the pictures in its folder, if it has company.
    fn folder_place(doc: &Doc, source: Option<&Path>) -> Option<(usize, usize)> {
        let path = source?;
        if doc.page_count.is_some() || !picture::is_picture(path) || !path.is_file() {
            return None;
        }
        let all = picture::in_folder(path);
        let here = all.iter().position(|p| p == path)?;
        (all.len() > 1).then_some((here + 1, all.len()))
    }

    /// Turns a page forward or back. On a picture file it moves to the next or
    /// previous picture in the folder instead: the reader closes and `main`
    /// opens that one, so each picture keeps its own place, ink and bookmarks.
    fn turn(&mut self, forward: bool) {
        if self.folder.is_some() {
            let step = if forward { 1 } else { -1 };
            if let Some(path) = self.options.picture_path.clone() {
                self.go = Some(Go::Neighbour { path, step });
                self.quit = true;
            }
        } else if forward {
            self.go_to_page(self.current + 1, false);
        } else {
            self.go_to_page(self.current.saturating_sub(1), false);
        }
    }

    /// Reads the watched file again once a change to it has settled.
    fn reload(&mut self) -> bool {
        if self.selection.is_some()
            || !matches!(self.dialog, Dialog::None)
            || !matches!(self.drag, Drag::Idle)
        {
            return false;
        }
        let Some(doc) = self
            .watch
            .as_mut()
            .and_then(|watch| watch.document(crate::load_file))
        else {
            return false;
        };
        if self.narrator.active() {
            self.narrator.stop();
            self.notice = Some("the file changed, so reading aloud stopped".into());
        }
        self.hold_anchor();
        self.doc = doc;
        if let Some(look) = self.shown_as {
            self.doc.source.set_look(look);
        }
        self.current = self.current.min(self.last());
        self.laid.clear();
        self.retain_protocols(|_| false);
        self.pixels.clear();
        self.selection = None;
        self.search = None;
        true
    }

    fn pages(&self) -> usize {
        self.doc.pages()
    }

    fn last(&self) -> usize {
        self.pages().saturating_sub(1)
    }

    fn image_size(&self, href: &str) -> Option<render::ImageSize> {
        match Doc::image_ref(href) {
            ImageRef::Embedded(id) => {
                let pixels = self.doc.source.image_size(id)?;
                Some(render::ImageSize {
                    pixels,
                    kind: if self.doc.source.is_page(id) {
                        render::ImageKind::Page
                    } else {
                        render::ImageKind::Figure
                    },
                })
            }
            ImageRef::Remote(url) => match self.remote.get(url) {
                Some(Remote::Ready(image)) => Some(image.dimensions().into()),
                _ => None,
            },
        }
    }

    fn markdown_colors(&self) -> render::MarkdownColors {
        let colors = self.look.colors();
        if self.options.colorful_markdown {
            render::MarkdownColors::colorful(colors)
        } else {
            render::MarkdownColors::uniform(colors.heading_color())
        }
    }

    fn laid(&mut self, page: usize) -> Rc<Laid> {
        if let Some((laid, _)) = self.laid.get(&page) {
            return laid.clone();
        }
        let markdown = self.doc.source.page(page);
        let font = self.picker.font_size();
        let sizes = |href: &str| self.image_size(href);
        let link_urls = if self.options.hide_link_urls {
            render::LinkUrls::Hide
        } else {
            render::LinkUrls::Show
        };
        let laid = Rc::new(render::layout_with_colors(
            &markdown,
            self.width,
            (font.width, font.height),
            &sizes,
            self.markdown_colors(),
            link_urls,
        ));
        self.laid.insert(page, (laid.clone(), markdown.is_empty()));
        laid
    }

    /// Rows above a page's text: top padding, or the page number between pages when scrolling.
    fn prefix(&self, page: usize) -> usize {
        if self.mode == Mode::Scroll && page > 0 && self.doc.page_count.is_some() {
            3
        } else {
            1
        }
    }

    fn height(&mut self, page: usize) -> usize {
        let laid = self.laid(page);
        // A page with nothing on it says so.
        self.prefix(page) + laid.rows.len().max(if self.blank(page) { 3 } else { 0 })
    }

    fn blank(&mut self, page: usize) -> bool {
        self.laid(page);
        self.laid[&page].1
    }

    fn view(&self) -> usize {
        self.view_height as usize
    }

    /// Furthest the current page scrolls in paged mode.
    fn max_offset(&mut self) -> usize {
        let height = self.height(self.current);
        height.saturating_sub(self.view())
    }

    /// Rows from the top of the screen to the end of the document, counting up to a screen's worth.
    fn rows_to_end(&mut self) -> usize {
        let mut rows = self.height(self.current).saturating_sub(self.offset);
        let mut page = self.current + 1;
        while rows <= self.view() && page < self.pages() {
            rows += self.height(page);
            page += 1;
        }
        rows
    }

    fn at_bottom(&mut self) -> bool {
        match self.mode {
            Mode::Paged => self.offset >= self.max_offset(),
            Mode::Scroll => self.rows_to_end() <= self.view(),
        }
    }

    /// Scrolls by `delta` rows; paged mode stays on its page.
    fn scroll_by(&mut self, delta: isize) {
        match self.mode {
            Mode::Paged => {
                let max = self.max_offset();
                self.offset = self.offset.saturating_add_signed(delta).min(max);
            }
            Mode::Scroll => {
                if delta >= 0 {
                    self.offset += delta as usize;
                    while self.current < self.last() && self.offset >= self.height(self.current) {
                        self.offset -= self.height(self.current);
                        self.current += 1;
                    }
                    self.clamp_end();
                } else {
                    let mut up = delta.unsigned_abs();
                    while up > self.offset && self.current > 0 {
                        up -= self.offset;
                        self.current -= 1;
                        self.offset = self.height(self.current);
                    }
                    self.offset = self.offset.saturating_sub(up);
                }
            }
        }
    }

    /// When scrolling, the end of the document stops at the bottom of the screen.
    fn clamp_end(&mut self) {
        let rows = self.rows_to_end();
        if rows < self.view() && (self.current > 0 || self.offset > 0) {
            let back = (self.view() - rows) as isize;
            self.scroll_by(-back);
        }
    }

    fn go_to_page(&mut self, page: usize, bottom: bool) {
        self.current = page.min(self.last());
        self.offset = 0;
        if bottom {
            self.offset = match self.mode {
                Mode::Paged => self.max_offset(),
                Mode::Scroll => self.height(self.current).saturating_sub(self.view()),
            };
        }
        if self.mode == Mode::Scroll {
            self.clamp_end();
        }
    }

    /// The page the reader is on: at the very end, the last page, even if it starts below the top.
    fn reading(&mut self) -> usize {
        if self.mode == Mode::Scroll && self.at_bottom() {
            self.last()
        } else {
            self.current
        }
    }

    fn footer(&mut self) -> String {
        let mut footer = self.searched();
        match self.tool {
            Tool::Select => {}
            Tool::Pen => {
                footer = format!("{footer} · pen {} · e eraser · esc stop", self.hue.name())
            }
            Tool::Eraser => footer = format!("{footer} · eraser · d pen · esc stop"),
            Tool::Move if self.picked.is_some() => {
                footer = format!(
                    "{footer} · drag or arrows move · corners or +/- resize · x delete · esc done"
                )
            }
            Tool::Move => footer = format!("{footer} · move: click ink to pick it · esc stop"),
        }
        let mark = match self.narrator.status() {
            Status::Idle => None,
            Status::Loading => Some("…"),
            Status::Speaking => Some("▶"),
            Status::Paused => Some("❚❚"),
        };
        if let Some(mark) = mark {
            footer = format!(
                "{footer} · {mark} {} {}",
                self.narrator.voice_label(),
                aloud::speed_label(self.narrator.speed())
            );
        }
        let hidden = self.no_ink().is_some() && {
            let pages: HashSet<usize> = self.screen.iter().flatten().map(|s| s.page).collect();
            self.strokes.iter().any(|i| pages.contains(&i.stroke.page))
        };
        if let Some(look) = self.shown_as {
            let images = match self.image_mode {
                ImageMode::Color => "i paint",
                ImageMode::Mono => "i real colors",
            };
            // While a drawing is picked, + and - resize it instead.
            let zoom = if self.picked.is_some() {
                ""
            } else {
                " · + - zoom"
            };
            footer = match look {
                crate::doc::Look::Original => {
                    format!("{footer}{zoom} · {images} · o terminal font")
                }
                crate::doc::Look::Terminal => format!("{footer} · o original look"),
            };
        }
        match &self.notice {
            Some(notice) => format!("{footer} · {notice}"),
            None if hidden => format!("{footer} · drawings here need Kitty graphics to show"),
            None => footer,
        }
    }

    /// The place, and how the search is going.
    fn searched(&mut self) -> String {
        let place = self.place();
        let Some(search) = &self.search else {
            return place;
        };
        let query = search.query.typed();
        match search.seek {
            Seek::Seeking {
                direction,
                from,
                step,
            } => {
                let page =
                    direction.page(from.page, step.min(self.pages()), self.pages().max(1)) + 1;
                format!("searching \"{query}\" · page {page} / {}", self.pages())
            }
            Seek::Found(_) => format!("{place} · \"{query}\" n/N"),
            Seek::Missing => format!("{place} · \"{query}\" not found"),
        }
    }

    /// Where the reader is: the page and how far through, or how far down a page-less document.
    fn place(&mut self) -> String {
        match self.doc.page_count {
            None => match self.folder {
                Some((here, all)) => format!("{here} / {all} · {}%", self.scrolled()),
                None => format!("{}%", self.scrolled()),
            },
            Some(count) => {
                let reading = self.reading();
                let progress = Progress::of(reading + 1, count);
                let marked = if self.bookmarked(reading) {
                    " · bookmarked"
                } else {
                    ""
                };
                format!(
                    "{} / {} · {}%{marked}",
                    progress.page, progress.page_count, progress.percent
                )
            }
        }
    }

    /// How far down a page-less document the screen is.
    fn scrolled(&mut self) -> usize {
        let max = self.height(0).saturating_sub(self.view());
        (self.offset * 100).checked_div(max).unwrap_or(100)
    }

    fn first_anchor(&mut self) -> Option<usize> {
        let laid = self.laid(self.current);
        let top = self
            .offset
            .saturating_sub(self.prefix(self.current))
            .min(laid.rows.len());
        laid.rows[top..]
            .iter()
            .flat_map(|row| &row.segs)
            .find_map(|seg| seg.offset)
    }

    fn top_anchor(&mut self) -> usize {
        self.first_anchor()
            .unwrap_or_else(|| self.laid(self.current).text.len())
    }

    fn reading_location(&mut self) -> Option<ReadingLocation> {
        let path = std::path::absolute(self.options.picture_path.as_deref()?).ok()?;
        let anchor = self.first_anchor().map(|offset| {
            let laid = self.laid(self.current);
            let end = (offset + HISTORY_ANCHOR_CHARS).min(laid.text.len());
            HistoryAnchor {
                offset,
                excerpt: laid.text[offset..end].to_vec(),
                page_hash: self.current_page_hash(),
            }
        });
        Some(ReadingLocation {
            path,
            viewport: Viewport {
                page: self.current,
                offset: self.offset,
                anchor,
                width: self.width,
                mode: self.mode,
                look: self.shown_as,
            },
        })
    }

    fn current_page_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.doc.source.page(self.current).hash(&mut hasher);
        hasher.finish()
    }

    fn restore_viewport(&mut self, viewport: Viewport) {
        self.current = viewport.page.min(self.last());
        self.offset = viewport.offset;
        if self.current != viewport.page {
            return;
        }
        let Some(anchor) = viewport.anchor else {
            return;
        };
        if anchor.page_hash != self.current_page_hash() {
            let laid = self.laid(self.current);
            let offset = laid
                .text
                .windows(anchor.excerpt.len())
                .enumerate()
                .filter(|(_, text)| *text == anchor.excerpt)
                .map(|(offset, _)| offset)
                .min_by_key(|offset| offset.abs_diff(anchor.offset));
            if let Some(offset) = offset {
                self.scroll_to_anchor(offset);
                return;
            }
        }
        if self.width != viewport.width || self.mode != viewport.mode {
            self.scroll_to_anchor(anchor.offset);
        }
    }

    fn scroll_to_anchor(&mut self, anchor: usize) {
        let laid = self.laid(self.current);
        let holds = |seg: &render::Seg| {
            seg.offset
                .is_some_and(|o| o + seg.text.chars().count() > anchor)
        };
        let row = laid
            .rows
            .iter()
            .position(|row| row.segs.iter().any(holds))
            .unwrap_or(laid.rows.len());
        let prefix = self.prefix(self.current);
        self.offset = row
            + if row == 0 {
                self.offset.min(prefix)
            } else {
                prefix
            };
    }

    fn hold_anchor(&mut self) {
        if self.mode == Mode::Scroll && self.width > 0 && self.anchor.is_none() {
            self.anchor = Some(self.top_anchor());
        }
    }

    /// Reports the place in a page-less document when its percent changes, or
    /// on `settling` (quitting) whenever it moved at all.
    fn report_scroll(&mut self, settling: bool) {
        let scroll = Scroll {
            anchor: self.top_anchor(),
            percent: self.scrolled(),
        };
        let due = match self.reported_scroll {
            None => true,
            Some(last) => last.percent != scroll.percent || (settling && last != scroll),
        };
        if due {
            self.reported_scroll = Some(scroll);
            (self.options.on_scroll)(scroll);
        }
    }

    fn report_page(&mut self) {
        let Some(count) = self.doc.page_count else {
            return self.report_scroll(false);
        };
        let progress = Progress::of(self.reading() + 1, count);
        if self.reported != Some(progress) {
            self.reported = Some(progress);
            (self.options.on_page)(progress);
        }
    }

    /// Lets go of laid-out pages and pixels far from the reader.
    fn prune(&mut self) {
        let (from, to) = (
            self.current.saturating_sub(KEPT_PAGES),
            self.current + KEPT_PAGES,
        );
        self.laid.retain(|page, _| (from..=to).contains(page));
    }

    fn receive_images(&mut self) {
        let mut arrived = false;
        while let Ok((url, image)) = self.fetched_rx.try_recv() {
            self.remote.insert(
                url,
                image.map_or(Remote::Failed, |i| Remote::Ready(Rc::new(i))),
            );
            arrived = true;
        }
        // A remote image's size is known once it arrives, so pages lay out again.
        if arrived {
            self.hold_anchor();
            self.laid.clear();
        }
    }

    fn loading(&self) -> bool {
        self.remote.values().any(|r| matches!(r, Remote::Loading))
    }

    fn ink(&mut self) -> Rgb {
        *self
            .ink
            .get_or_insert_with(|| query_color(10).unwrap_or(FALLBACK_INK))
    }

    fn paper(&mut self) -> Rgb {
        *self
            .paper
            .get_or_insert_with(|| query_color(11).unwrap_or(FALLBACK_PAPER))
    }

    /// The pixels of a figure in the current image mode, decoding or fetching as needed.
    fn figure_pixels(&mut self, href: &str, cols: u16) -> Option<Rc<RgbaImage>> {
        let key = (href.to_owned(), self.image_mode);
        if let Some(pixels) = self.pixels.get(&key) {
            return pixels.clone();
        }
        let width = cols as u32 * self.picker.font_size().width as u32;
        let original = match Doc::image_ref(href) {
            // A diagram is drawn in the terminal's colors, so it isn't painted.
            ImageRef::Embedded(id) if self.doc.source.is_diagram(id) => {
                let (ink, paper) = (self.ink(), self.paper());
                let pixels = self.doc.source.draw(id, width, ink, paper).map(Rc::new);
                self.pixels.insert(key, pixels.clone());
                return pixels;
            }
            ImageRef::Embedded(id) => match self.doc.source.decode(id, width) {
                Some(image) => Rc::new(image),
                None => {
                    self.pixels.insert(key, None);
                    return None;
                }
            },
            ImageRef::Remote(url) => match self.remote.get(url) {
                Some(Remote::Ready(image)) => image.clone(),
                Some(_) => return None,
                None => {
                    self.remote.insert(url.to_owned(), Remote::Loading);
                    let (tx, url) = (self.fetched_tx.clone(), url.to_owned());
                    std::thread::spawn(move || {
                        let image = article::fetch_image(&url);
                        let _ = tx.send((url, image));
                    });
                    return None;
                }
            },
        };
        let pixels = match self.image_mode {
            ImageMode::Color => original,
            ImageMode::Mono => {
                let ink = self.ink();
                Rc::new(mono::paint(&original, ink))
            }
        };
        self.pixels.insert(key, Some(pixels.clone()));
        Some(pixels)
    }

    // -----------------------------------------------------------------------
    // Drawing.

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.room = area.width.saturating_sub(4);
        let width = self.reading_width.within(self.room);
        let mode = if self.doc.page_count.is_some()
            && area.width >= PAGED_MIN_WIDTH
            && area.height >= PAGED_MIN_HEIGHT
        {
            Mode::Paged
        } else {
            Mode::Scroll
        };
        if width != self.width || mode != self.mode {
            self.hold_anchor();
            self.width = width;
            self.laid.clear();
            self.retain_protocols(|_| false);
            self.pixels.clear();
            if mode != self.mode {
                self.mode = mode;
                self.offset = 0;
            }
        }
        self.view_height = area.height.saturating_sub(FOOTER_ROWS).max(1);
        let anchor = self.anchor.take();
        if let Some(viewport) = self.options.start_viewport.take() {
            self.restore_viewport(viewport);
        } else if let Some(anchor) = anchor {
            self.scroll_to_anchor(anchor);
        }
        self.offset = match self.mode {
            Mode::Paged => self.offset.min(self.max_offset()),
            Mode::Scroll => self.offset.min(self.height(self.current).saturating_sub(1)),
        };
        if self.mode == Mode::Scroll {
            self.clamp_end();
        }
        self.prune();
        self.column = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y,
            width,
            height: self.view_height,
        };

        // What each row of the screen shows.
        let mut screen = Vec::with_capacity(self.view());
        let (mut page, mut row) = (self.current, self.offset);
        while screen.len() < self.view() && page < self.pages() {
            if row < self.height(page) {
                screen.push(Some(ScreenRow { page, row }));
                row += 1;
            } else if self.mode == Mode::Paged {
                break;
            } else {
                page += 1;
                row = 0;
            }
        }
        screen.resize(self.view(), None);
        self.screen = screen;
        let ink = self.look.ink();
        if let Some(ink) = ink {
            art::curtains(f.buffer_mut(), area, self.column, ink);
        }

        let selection = self.selection.map(Selection::range);
        let screen = self.screen.clone();
        let found = self.matches_on(&screen);
        for (y, slot) in screen.iter().enumerate() {
            let Some(ScreenRow { page, row }) = *slot else {
                continue;
            };
            let marks = Marks {
                selection,
                found: found.get(&page).map_or(&[], Vec::as_slice),
                here: self.found_here(),
                said: self.narrator.said(),
            };
            self.draw_row(f, page, row, self.column.y + y as u16, marks);
        }
        self.draw_figures(f, &screen);
        self.draw_strokes(area);

        // The footer sits on a bar that dissolves up into the page.
        if let (true, Some(ink)) = (area.height >= FOOTER_ROWS, ink) {
            let bar = Rect {
                y: area.bottom() - 2,
                height: 2,
                ..area
            };
            art::surface(f.buffer_mut(), bar, ink);
            let edge = Rect {
                y: bar.y - 1,
                height: 1,
                ..area
            };
            art::dissolve(f.buffer_mut(), edge, ink, art::Bar::Below);
        }
        let footer = self.footer();
        let footer_area = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(2),
            width: area.width,
            height: 1.min(area.height),
        };
        f.render_widget(
            Paragraph::new(footer)
                .style(Style::default().fg(DIM))
                .alignment(Alignment::Center),
            footer_area,
        );

        let panel = match self.dialog.clone() {
            Dialog::None => None,
            Dialog::Help => Some(self.draw_help(f, area)),
            Dialog::Guide => Some(self.draw_guide(f, area)),
            Dialog::Goto { digits, page_count } => {
                Some(self.draw_goto(f, area, &digits, page_count))
            }
            Dialog::Find { typed } => Some(self.draw_find(f, area, &typed)),
            Dialog::Ask { typed, passage } => Some(self.draw_ask(f, area, &typed, &passage)),
            Dialog::Bookmarks { selected } => Some(self.draw_bookmarks(f, area, selected)),
            Dialog::Voices { typed, selected } => Some(self.draw_voices(f, area, &typed, selected)),
        };
        let news = self.news.as_mut().map(|news| news.draw(f, ink));
        // Kitty images sit above the text, so any under a panel would hide it.
        if let Some(panel) = panel {
            if let Some(ink) = ink {
                art::shadow(f.buffer_mut(), panel, ink);
            }
            self.placements.retain(|(_, rect)| !rect.intersects(panel));
        }
        if let Some(news) = news {
            self.placements.retain(|(_, rect)| !rect.intersects(news));
        }
    }

    /// Every match of the search on the pages on screen.
    fn matches_on(&mut self, screen: &[Option<ScreenRow>]) -> HashMap<usize, Vec<(usize, usize)>> {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return HashMap::new();
        };
        let mut found = HashMap::new();
        for page in screen.iter().flatten().map(|s| s.page) {
            found
                .entry(page)
                .or_insert_with(|| page_matches(&query, &self.laid(page)));
        }
        found
    }

    fn found_here(&self) -> Option<Match> {
        match self.search.as_ref()?.seek {
            Seek::Found(here) => Some(here),
            _ => None,
        }
    }

    fn draw_row(&mut self, f: &mut Frame, page: usize, row: usize, y: u16, marks: Marks) {
        let prefix = self.prefix(page);
        let column = self.column;
        let buf = f.buffer_mut();
        if row < prefix {
            if prefix == 3 && row == 1 {
                let label = format!("·  {}  ·", page + 1);
                let label_width = label.chars().count() as u16;
                let x = column.x + column.width.saturating_sub(label_width) / 2;
                buf.set_string(x, y, label, Style::default().fg(DIM));
                let row = Rect {
                    y,
                    height: 1,
                    ..column
                };
                if let Some(ink) = self.tints() {
                    art::rule(buf, row, label_width, ink);
                }
            }
            return;
        }
        let laid = self.laid(page);
        if laid.rows.is_empty() && self.blank(page) {
            if row == prefix + 1 {
                let label = "blank page";
                buf.set_string(
                    column.x + (column.width.saturating_sub(10)) / 2,
                    y,
                    label,
                    Style::default().fg(DIM),
                );
            }
            return;
        }
        let Some(line) = laid.rows.get(row - prefix) else {
            return;
        };
        let highlights: Vec<&Highlight> =
            self.highlights.iter().filter(|h| h.page == page).collect();
        let buf = f.buffer_mut();
        for seg in &line.segs {
            let mut x = column.x + seg.x;
            for (i, c) in seg.text.chars().enumerate() {
                let mut style = seg.style;
                if let Some(offset) = seg.offset.map(|o| o + i) {
                    let at = TextPos { page, offset };
                    if let Some(h) = highlights
                        .iter()
                        .find(|h| h.start <= offset && offset < h.end)
                    {
                        style = style.bg(h.hue.marker()).fg(MARK_INK);
                    }
                    if marks
                        .found
                        .iter()
                        .any(|&(start, end)| start <= offset && offset < end)
                    {
                        let here = marks
                            .here
                            .is_some_and(|m| m.page == page && m.start <= offset && offset < m.end);
                        style = style.bg(if here { FOUND_HERE } else { FOUND }).fg(MARK_INK);
                    }
                    // The word being read aloud, in the terminal's own colors turned over.
                    if marks
                        .said
                        .is_some_and(|s| s.page == page && s.start <= offset && offset < s.end)
                    {
                        style = style
                            .fg(Color::Reset)
                            .bg(Color::Reset)
                            .add_modifier(Modifier::REVERSED | Modifier::BOLD);
                    }
                    if marks
                        .selection
                        .is_some_and(|(start, end)| start <= at && at < end)
                    {
                        style = style.bg(SELECTION).fg(Color::Reset);
                    }
                }
                if x < column.right() {
                    buf.set_string(x, y, c.to_string(), style);
                }
                x += char_width(c);
            }
        }
    }

    /// Draws the visible part of each figure on screen.
    fn draw_figures(&mut self, f: &mut Frame, screen: &[Option<ScreenRow>]) {
        self.placements.clear();
        let mut used: HashSet<ProtocolKey> = HashSet::new();
        let mut pixels_used: HashSet<(String, ImageMode)> = HashSet::new();
        let pages: Vec<usize> = {
            let mut pages: Vec<usize> = screen.iter().flatten().map(|s| s.page).collect();
            pages.dedup();
            pages
        };
        for page in pages {
            let laid = self.laid(page);
            let prefix = self.prefix(page);
            for figure in &laid.figures {
                let top = prefix + figure.top;
                let visible: Vec<usize> = screen
                    .iter()
                    .enumerate()
                    .filter_map(|(y, s)| {
                        s.filter(|s| {
                            s.page == page && s.row >= top && s.row < top + figure.rows as usize
                        })
                        .map(|_| y)
                    })
                    .collect();
                let (Some(&first_y), Some(&last_y)) = (visible.first(), visible.last()) else {
                    continue;
                };
                let from = (screen[first_y].map_or(top, |s| s.row) - top) as u16;
                let to = from + (last_y - first_y + 1) as u16;
                let pixels = self.figure_pixels(&figure.href, figure.cols);
                pixels_used.insert((figure.href.clone(), self.image_mode));
                let Some(pixels) = pixels else {
                    // An image still downloading shows a moving field in its place.
                    let arriving = matches!(
                        Doc::image_ref(&figure.href),
                        ImageRef::Remote(url) if matches!(self.remote.get(url), Some(Remote::Loading))
                    );
                    if let (true, Some(ink)) = (arriving, self.tints()) {
                        let rect = Rect {
                            x: self.column.x + figure.x,
                            y: self.column.y + first_y as u16,
                            width: figure.cols,
                            height: (last_y - first_y + 1) as u16,
                        };
                        art::field(f.buffer_mut(), rect, ink, Field::Noise, art::clock());
                    }
                    continue;
                };
                let key = ProtocolKey {
                    href: figure.href.clone(),
                    mode: self.image_mode,
                    cols: figure.cols,
                    from,
                    to,
                };
                if !self.protocols.contains_key(&key) {
                    // Only the rows on screen are drawn: protocols can't clip an image's top.
                    let (w, h) = pixels.dimensions();
                    let rows = figure.rows as u32;
                    let (y0, y1) = (
                        h * from as u32 / rows,
                        (h * to as u32 / rows)
                            .max(h * from as u32 / rows + 1)
                            .min(h),
                    );
                    let slice =
                        image::imageops::crop_imm(pixels.as_ref(), 0, y0, w, y1 - y0).to_image();
                    // Protocols never scale an image up, so it's sized to fill its cells first.
                    let font = self.picker.font_size();
                    let (box_w, box_h) = (
                        figure.cols as f32 * font.width as f32,
                        (to - from) as f32 * font.height as f32,
                    );
                    let scale = (box_w / slice.width() as f32).min(box_h / slice.height() as f32);
                    let fitted = (
                        ((slice.width() as f32 * scale) as u32).max(1),
                        ((slice.height() as f32 * scale) as u32).max(1),
                    );
                    let slice = if fitted == slice.dimensions() {
                        slice
                    } else {
                        image::imageops::resize(
                            &slice,
                            fitted.0,
                            fitted.1,
                            image::imageops::FilterType::Triangle,
                        )
                    };
                    let drawn = match &mut self.kitty {
                        Some(kitty) => {
                            let cells = |px: u32, cell: u16, max: u16| {
                                (px.div_ceil(cell.max(1) as u32) as u16).clamp(1, max)
                            };
                            let (cols, rows) = (
                                cells(slice.width(), font.width, figure.cols),
                                cells(slice.height(), font.height, to - from),
                            );
                            kitty.upload(&slice, cols, rows, 0).map(Drawn::Kitty)
                        }
                        None => self
                            .picker
                            .new_protocol(
                                DynamicImage::ImageRgba8(slice),
                                Size::new(figure.cols, to - from),
                                Resize::Fit(None),
                            )
                            .ok()
                            .map(Drawn::Protocol),
                    };
                    let Some(drawn) = drawn else { continue };
                    self.protocols.insert(key.clone(), drawn);
                }
                let Some(drawn) = self.protocols.get(&key) else {
                    continue;
                };
                let size = match drawn {
                    Drawn::Kitty(image) => Size::new(image.cols, image.rows),
                    Drawn::Protocol(protocol) => protocol.size(),
                };
                let x = self.column.x + figure.x + figure.cols.saturating_sub(size.width) / 2;
                // A figure cut off at the top sits against the bottom of its visible rows.
                let y = if from > 0 {
                    self.column.y + last_y as u16 + 1 - size.height.min(to - from)
                } else {
                    self.column.y + first_y as u16
                };
                let rect = Rect {
                    x,
                    y,
                    width: size.width,
                    height: size.height.min(to - from),
                };
                match drawn {
                    Drawn::Kitty(image) => self.placements.push((*image, rect)),
                    Drawn::Protocol(protocol) => {
                        f.render_widget(Image::new(protocol).allow_clipping(true), rect)
                    }
                }
                used.insert(key);
            }
        }
        self.retain_protocols(|key| used.contains(key));
        self.pixels.retain(|key, _| pixels_used.contains(key));
    }

    /// The redraw after a resize clears the screen, and kitty's placements with
    /// it. Rex and Ghostty drop the uploaded figures too, so placing the old
    /// ids again shows nothing: every figure and stroke is uploaded afresh.
    fn resized(&mut self) {
        if let Some(kitty) = &mut self.kitty {
            kitty.reset();
            for (_, image) in self.ink_drawn.drain() {
                kitty.free(image);
            }
        }
        self.retain_protocols(|_| false);
    }

    /// Keeps the drawn figures `keep` accepts, freeing the terminal's copy of the rest.
    fn retain_protocols(&mut self, keep: impl Fn(&ProtocolKey) -> bool) {
        let kitty = &mut self.kitty;
        self.protocols.retain(|key, drawn| {
            let kept = keep(key);
            if let (false, Drawn::Kitty(image), Some(kitty)) = (kept, &drawn, kitty.as_mut()) {
                kitty.free(*image);
            }
            kept
        });
    }

    /// The dither's tints, or `None` with the classic UI.
    fn tints(&self) -> Option<art::Ink> {
        self.look.ink()
    }

    fn panel(
        f: &mut Frame,
        area: Rect,
        ink: Option<art::Ink>,
        title: &str,
        lines: Vec<Line>,
        width: u16,
    ) -> Rect {
        let height = lines.len() as u16 + 4;
        let width = (width + 8).min(area.width);
        let rect = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height: height.min(area.height),
        };
        f.render_widget(Clear, rect);
        if let Some(ink) = ink {
            art::surface(f.buffer_mut(), rect, ink);
        }
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
        let body = Rect {
            x: inner.x + 3,
            y: inner.y + 1,
            width: inner.width.saturating_sub(6),
            height: inner.height.saturating_sub(2),
        };
        f.render_widget(Paragraph::new(lines), body);
        rect
    }

    fn help_rows(&self) -> Vec<(&'static str, &'static str)> {
        let paged = self.doc.page_count.is_some();
        let mut rows = Vec::new();
        if paged {
            rows.push(("←/→  h/l", "previous / next page"));
        } else if self.folder.is_some() {
            rows.push(("←/→  h/l  p/n", "previous / next picture in the folder"));
        }
        rows.extend([
            ("H/L", "back / forward through followed links"),
            ("j/k  ↓/↑", "scroll"),
            ("^d  u", "half screen down / up"),
            ("space/b", "next / previous screen"),
        ]);
        rows.push((
            "g/G",
            if paged {
                "first / last page"
            } else {
                "top / bottom"
            },
        ));
        if paged {
            rows.push(("t  :", "go to page"));
            rows.extend([
                ("m", "bookmark this page, or remove it"),
                ("'", "bookmarks"),
            ]);
        }
        rows.extend([("f  F", "find text"), ("n/N", "next / previous match")]);
        rows.extend([
            ("a", "read aloud from here; pause / go on"),
            ("A", "stop reading aloud"),
            ("[  ]", "read slower / faster"),
            (",  .", "previous / next sentence aloud"),
            ("s", "pick a voice"),
            ("<  >  =", "narrower / wider / fit column (- + too)"),
        ]);
        if let Some(look) = self.shown_as {
            rows.push((
                "o",
                match look {
                    crate::doc::Look::Original => "show in the terminal's font",
                    crate::doc::Look::Terminal => "show as designed",
                },
            ));
        }
        rows.push((
            "i",
            if self.image_mode == ImageMode::Color {
                "paint images"
            } else {
                "show original images"
            },
        ));
        rows.extend([
            ("drag  click", "select and copy text; click follows a link"),
            ("@  ^enter", "ask an agent about the selection (⇧⌘A in Rex)"),
            ("y", "highlight selection, or clear one"),
            ("1-6", "pick a color; paints the selection"),
            ("d  e", "pen / eraser: drag to draw or erase"),
            ("v", "move tool: pick, move, resize ink"),
            ("!", "how to highlight and draw"),
            ("?  q", "help / quit"),
        ]);
        rows
    }

    fn draw_help(&self, f: &mut Frame, area: Rect) -> Rect {
        let rows = self.help_rows();
        let width = 12
            + rows
                .iter()
                .map(|(_, a)| a.chars().count() as u16)
                .max()
                .unwrap_or(0);
        let lines = rows
            .into_iter()
            .map(|(keys, action)| {
                Line::from(vec![
                    Span::styled(
                        format!("{keys:<12}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(action, Style::default().fg(DIM)),
                ])
            })
            .collect();
        Self::panel(f, area, self.tints(), "Keys", lines, width)
    }

    /// How to mark up a document: highlights, colors, the pen and the eraser.
    fn draw_guide(&self, f: &mut Frame, area: Rect) -> Rect {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(DIM);
        let heading = |text: &'static str| Line::from(Span::styled(text, bold));
        let step = |keys: &'static str, action: &'static str| {
            Line::from(vec![
                Span::styled(format!("  {keys:<10}"), bold),
                Span::styled(action, dim),
            ])
        };
        let swatches: Vec<Span> = Hue::ALL
            .iter()
            .zip(1..)
            .flat_map(|(hue, n)| {
                [
                    Span::styled(
                        format!(" {n} {} ", hue.name()),
                        Style::default().bg(hue.marker()).fg(MARK_INK),
                    ),
                    Span::raw(" "),
                ]
            })
            .collect();
        let mut lines = vec![
            heading("Highlight"),
            step("drag", "select text; it's copied as you let go"),
            step("@", "ask an open agent about that selection"),
            step("y", "highlight it, or clear a highlight it touches"),
            Line::raw(""),
            heading("Colors"),
            Line::from([vec![Span::raw("  ")], swatches].concat()),
            step("1-6", "pick a color; paints a selection too"),
            Line::raw(""),
            heading("Draw"),
            step("d", "pen: drag anywhere to draw"),
            step("e", "eraser: drag over a stroke to rub it out"),
            step("esc", "back to selecting text"),
            step("", "lines, boxes, triangles and circles snap clean"),
            step("", "ink sticks to the word it starts on"),
            Line::raw(""),
            heading("Move and resize"),
            step("v", "move tool: click a stroke to pick it"),
            step("drag", "move it; drag a corner to resize"),
            step("←↓↑→ +-", "nudge (or hjkl) · bigger / smaller"),
            step("1-6  x", "recolor · delete · esc lets go"),
        ];
        if self.no_ink().is_some() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "  Drawing needs Kitty graphics, and not inside tmux.",
                dim,
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  saved as you go · esc close",
            dim,
        )));
        Self::panel(f, area, self.tints(), "Mark up", lines, 58)
    }

    fn draw_goto(&self, f: &mut Frame, area: Rect, digits: &str, page_count: usize) -> Rect {
        let hint = "enter go · esc cancel";
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    digits.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("▏"),
                Span::styled(format!(" / {page_count}"), Style::default().fg(DIM)),
            ])
            .centered(),
            Line::default(),
            Line::from(Span::styled(hint, Style::default().fg(DIM))).centered(),
        ];
        Self::panel(
            f,
            area,
            self.tints(),
            "Go to page",
            lines,
            hint.chars().count() as u16,
        )
    }

    fn draw_find(&self, f: &mut Frame, area: Rect, typed: &str) -> Rect {
        let hint = "enter find · esc cancel";
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    typed.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("▏"),
            ]),
            Line::default(),
            Line::from(Span::styled(hint, Style::default().fg(DIM))).centered(),
        ];
        let width = (hint.chars().count() as u16).max(typed.chars().count() as u16 + 1);
        Self::panel(f, area, self.tints(), "Find", lines, width)
    }

    fn draw_ask(&self, f: &mut Frame, area: Rect, typed: &str, passage: &str) -> Rect {
        let hint = "enter ask · alt+enter line · esc cancel";
        let preview: String = passage.chars().take(42).collect();
        let lines = vec![
            Line::from(Span::styled(
                format!("“{preview}”"),
                Style::default().fg(DIM),
            )),
            Line::default(),
            Line::from(vec![
                Span::styled(
                    typed.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw("▏"),
            ]),
            Line::default(),
            Line::from(Span::styled(hint, Style::default().fg(DIM))).centered(),
        ];
        let width = (hint.chars().count() as u16)
            .max(preview.chars().count() as u16 + 2)
            .max(typed.chars().count() as u16 + 1);
        Self::panel(f, area, self.tints(), "Ask", lines, width)
    }

    fn draw_bookmarks(&self, f: &mut Frame, area: Rect, selected: usize) -> Rect {
        let hint = "enter go · x remove · esc close";
        if self.bookmarks.is_empty() {
            let lines = vec![
                Line::from(Span::styled(
                    "No bookmarks yet. Press m to mark a page.",
                    Style::default().fg(DIM),
                ))
                .centered(),
            ];
            return Self::panel(f, area, self.tints(), "Bookmarks", lines, 42);
        }
        // As many as fit, keeping the picked one in view.
        let room = (area.height as usize).saturating_sub(8).max(1);
        let first = selected
            .saturating_sub(room - 1)
            .min(self.bookmarks.len().saturating_sub(room));
        let number_width = self.pages().to_string().len();
        let mut lines: Vec<Line> = self.bookmarks[first..]
            .iter()
            .take(room)
            .enumerate()
            .map(|(i, b)| {
                let picked = first + i == selected;
                let style = if picked {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(DIM)
                };
                Line::from(vec![
                    Span::styled(if picked { "› " } else { "  " }, style),
                    Span::styled(
                        format!("{:>number_width$}  ", b.page + 1),
                        Style::default().fg(DIM),
                    ),
                    Span::styled(b.label.clone(), style),
                ])
            })
            .collect();
        let width = lines
            .iter()
            .map(|l| l.width() as u16)
            .max()
            .unwrap_or(0)
            .max(hint.chars().count() as u16);
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(hint, Style::default().fg(DIM))).centered());
        Self::panel(f, area, self.tints(), "Bookmarks", lines, width)
    }

    /// The voice picker: a search box over every voice, or a word that
    /// they're on their way. The list keeps its size as the search narrows it.
    fn draw_voices(
        &mut self,
        f: &mut Frame,
        area: Rect,
        typed: &str,
        selected: Option<usize>,
    ) -> Rect {
        let hint = "↑/↓ pick · enter use · esc close";
        let current = self.narrator.voice().to_owned();
        let search = Line::from(vec![
            Span::styled("search  ", Style::default().fg(DIM)),
            Span::styled(
                typed.to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("▏"),
            Span::styled(
                if typed.is_empty() {
                    "name, language or gender"
                } else {
                    ""
                },
                Style::default().fg(DIM),
            ),
        ]);
        let Some(voices) = self.narrator.voices() else {
            let lines = vec![
                search,
                Line::default(),
                Line::from(Span::styled("Loading voices…", Style::default().fg(DIM))),
            ];
            return Self::panel(f, area, self.tints(), "Voice", lines, 52);
        };
        let ranked = aloud::rank(voices, typed);
        let picked = picked_voice(&ranked, &current, typed, selected);
        let room = (area.height as usize)
            .saturating_sub(12)
            .clamp(1, voices.len().max(1));
        let first = picked
            .saturating_sub(room - 1)
            .min(ranked.len().saturating_sub(room));
        // Sized for every voice, so the panel holds still while typing.
        let name_width = voices
            .iter()
            .map(|v| v.short().chars().count())
            .max()
            .unwrap_or(0);
        let about_width = voices
            .iter()
            .map(|v| v.language.chars().count() + v.gender.chars().count() + 3)
            .max()
            .unwrap_or(0);
        let mut lines = vec![
            search,
            Line::from(Span::styled(
                format!("{} of {}", ranked.len(), voices.len()),
                Style::default().fg(DIM),
            )),
            Line::default(),
        ];
        if ranked.is_empty() {
            lines.push(Line::from(Span::styled(
                "No voice matches. Try a name, language or gender.",
                Style::default().fg(DIM),
            )));
        }
        lines.extend(
            ranked
                .iter()
                .enumerate()
                .skip(first)
                .take(room)
                .map(|(i, v)| {
                    let style = if i == picked {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let mark = match (i == picked, v.name == current) {
                        (true, _) => "› ",
                        (false, true) => "• ",
                        (false, false) => "  ",
                    };
                    Line::from(vec![
                        Span::styled(mark, style),
                        Span::styled(format!("{:<name_width$}  ", v.short()), style),
                        Span::styled(
                            format!("{} · {}", v.language, v.gender.to_lowercase()),
                            Style::default().fg(DIM),
                        ),
                    ])
                }),
        );
        let shown = ranked.len().saturating_sub(first).min(room).max(1);
        lines.extend((shown..room).map(|_| Line::default()));
        let width = (name_width + about_width + 4).max(hint.chars().count()) as u16;
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(hint, Style::default().fg(DIM))).centered());
        Self::panel(f, area, self.tints(), "Voice", lines, width)
    }

    /// Typing in the voice picker searches; enter reads in the picked voice
    /// from the word being read. While the list loads, only the search works.
    fn voices_key(&mut self, key: &KeyEvent, typed: String, selected: Option<usize>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let current = self.narrator.voice().to_owned();
        let pick = self.narrator.voices().map(|voices| {
            let ranked = aloud::rank(voices, &typed);
            let at = picked_voice(&ranked, &current, &typed, selected);
            (
                at,
                ranked.len(),
                ranked
                    .get(at)
                    .map(|v| (v.name.clone(), v.short().to_owned())),
            )
        });
        let up = matches!(key.code, KeyCode::Up | KeyCode::BackTab)
            || (ctrl && matches!(key.code, KeyCode::Char('p' | 'k')));
        let down = matches!(key.code, KeyCode::Down | KeyCode::Tab)
            || (ctrl && matches!(key.code, KeyCode::Char('n' | 'j')));
        match (key.code, pick) {
            (KeyCode::Esc, _) => self.dialog = Dialog::None,
            (KeyCode::Enter, Some((_, _, Some((name, short))))) => {
                self.dialog = Dialog::None;
                self.use_voice(name, &short);
            }
            (_, Some((at, _, _))) if up => {
                self.dialog = Dialog::Voices {
                    typed,
                    selected: Some(at.saturating_sub(1)),
                }
            }
            (_, Some((at, count, _))) if down => {
                self.dialog = Dialog::Voices {
                    typed,
                    selected: Some((at + 1).min(count.saturating_sub(1))),
                }
            }
            (KeyCode::Backspace, _) => {
                let mut typed = typed;
                typed.pop();
                self.dialog = Dialog::Voices {
                    typed,
                    selected: None,
                };
            }
            (KeyCode::Char(c), _) if !ctrl => {
                self.dialog = Dialog::Voices {
                    typed: format!("{typed}{c}"),
                    selected: None,
                }
            }
            _ => {}
        }
    }

    /// Reads in `name` from now on, and remembers it for next time.
    fn use_voice(&mut self, name: String, short: &str) {
        (self.options.on_voice)(&name);
        let reading = self.narrator.active();
        self.narrator.set_voice(name);
        self.follow = true;
        self.notice = Some(if reading {
            format!("reading in {short}")
        } else {
            format!("{short} reads next · a to start")
        });
    }

    /// Collects the command line Rex types after F12; enter carries it out.
    fn rex_key(&mut self, key: &KeyEvent, mut line: String) {
        match key.code {
            KeyCode::Enter => {
                self.rex_command = None;
                self.obey_rex(remote::Command::parse(&line));
            }
            KeyCode::Esc => self.rex_command = None,
            KeyCode::Char(c) if line.len() < remote::LONGEST => {
                line.push(c);
                self.rex_command = Some(line);
            }
            _ => self.rex_command = Some(line),
        }
    }

    /// Carries out what Rex's command palette asked for.
    fn obey_rex(&mut self, command: Result<Command, String>) {
        self.notice = None;
        {
            match command {
                Ok(Command::Ask(question)) => {
                    let passage = self.selected_text();
                    if passage.trim().is_empty() {
                        self.notice = Some("select some text, then ask".into());
                    } else if question.trim().is_empty() {
                        self.open_ask();
                    } else {
                        self.dialog = Dialog::None;
                        self.submit_ask(&passage, &question);
                    }
                }
                Ok(Command::Play) => self.read_aloud(),
                Ok(Command::Stop) => {
                    if self.narrator.active() {
                        self.narrator.stop();
                    }
                    self.notice = Some("stopped reading aloud".into());
                }
                Ok(Command::Faster) => self.change_speed(true),
                Ok(Command::Slower) => self.change_speed(false),
                Ok(Command::Next) => self.skip_sentence(Direction::Forward),
                Ok(Command::Previous) => self.skip_sentence(Direction::Backward),
                Ok(Command::Voices) => self.open_voices(),
                Ok(Command::Quit) => self.quit = true,
                Ok(Command::Voice(name)) => {
                    let short = edge::built_in()
                        .iter()
                        .chain(self.narrator.voices().unwrap_or(&[]))
                        .find(|voice| voice.name == name)
                        .map(|voice| voice.short().to_owned())
                        .unwrap_or_else(|| name.clone());
                    self.use_voice(name, &short);
                }
                Err(message) => self.notice = Some(message),
            }
        }
    }

    /// Opens the voice picker on the voice reading now.
    fn open_voices(&mut self) {
        self.narrator.prepare_voices();
        self.dialog = Dialog::Voices {
            typed: String::new(),
            selected: None,
        };
    }

    // -----------------------------------------------------------------------
    // Reading aloud.

    /// Where the text on screen starts: the sentence the top line is in, if it
    /// began just above.
    fn screen_top(&mut self) -> Spot {
        for slot in self.screen.clone().into_iter().flatten() {
            let prefix = self.prefix(slot.page);
            if slot.row < prefix {
                continue;
            }
            let laid = self.laid(slot.page);
            let first = laid
                .rows
                .get(slot.row - prefix)
                .and_then(|row| row.segs.iter().find_map(|seg| seg.offset));
            if let Some(offset) = first {
                let start = aloud::sentence_start(&laid.text, offset);
                return Spot {
                    page: slot.page,
                    offset: if offset - start <= SPEECH_SENTENCE_REWIND {
                        start
                    } else {
                        offset
                    },
                };
            }
        }
        Spot {
            page: self.current,
            offset: 0,
        }
    }

    /// The screen row character `offset` of `page` is on, as last drawn.
    fn screen_row(&mut self, page: usize, offset: usize) -> Option<usize> {
        let row = self.laid(page).locate(offset).0 + self.prefix(page);
        self.screen
            .iter()
            .position(|s| s.is_some_and(|s| s.page == page && s.row == row))
    }

    /// Scrolls so character `offset` of `page` sits a third of the way down.
    fn show(&mut self, page: usize, offset: usize) {
        let row = self.laid(page).locate(offset).0 + self.prefix(page);
        self.current = page.min(self.last());
        let offset = row.saturating_sub(self.view() / 3);
        match self.mode {
            Mode::Paged => self.offset = offset.min(self.max_offset()),
            Mode::Scroll => {
                self.offset = offset;
                self.clamp_end();
            }
        }
    }

    /// `a`: reads aloud from the selection, from where reading stopped if
    /// that's on screen, or from the top of the screen; while reading, pauses
    /// and carries on.
    fn read_aloud(&mut self) {
        self.follow = true;
        if self.narrator.active() {
            return self.narrator.toggle();
        }
        // A page drawn as designed is a picture, with no words to read, so
        // reading aloud shows the words and starts at the top of the page.
        if self.shown_as == Some(crate::doc::Look::Original) {
            self.toggle_look();
            self.notice = Some("reading the text aloud; o shows the design again".into());
            self.narrator.start(Spot {
                page: self.current,
                offset: 0,
            });
            return;
        }
        let from = match self.selection.map(Selection::range) {
            Some((start, end)) if start != end => {
                self.selection = None;
                Spot {
                    page: start.page,
                    offset: start.offset,
                }
            }
            _ => match self.narrator.resume() {
                Some(spot) if self.screen_row(spot.page, spot.offset).is_some() => spot,
                _ => self.screen_top(),
            },
        };
        self.narrator.start(from);
    }

    /// `,` and `.`: back to the start of this sentence (or the one before,
    /// just after one starts), or on to the next.
    fn skip_sentence(&mut self, direction: Direction) {
        let Some(here) = self.narrator.here() else {
            return;
        };
        let laid = self.laid(here.page);
        let text = &laid.text;
        let target = match direction {
            Direction::Forward => {
                let end = aloud::sentence_end(text, here.offset);
                if text[end.min(text.len())..]
                    .iter()
                    .any(|c| c.is_alphanumeric())
                {
                    Spot {
                        page: here.page,
                        offset: end,
                    }
                } else if here.page < self.last() {
                    Spot {
                        page: here.page + 1,
                        offset: 0,
                    }
                } else {
                    return;
                }
            }
            Direction::Backward => {
                let start = aloud::sentence_start(text, here.offset);
                // Just into a sentence, back goes to the one before.
                let words_in = text[start..here.offset.min(text.len())]
                    .iter()
                    .filter(|c| c.is_whitespace())
                    .count();
                if words_in >= 2 {
                    Spot {
                        page: here.page,
                        offset: start,
                    }
                } else if start > 0 {
                    let mut before = start;
                    while before > 0 && text[before - 1].is_whitespace() {
                        before -= 1;
                    }
                    Spot {
                        page: here.page,
                        offset: aloud::sentence_start(text, before.saturating_sub(1)),
                    }
                } else if here.page > 0 {
                    let previous = self.laid(here.page - 1);
                    let last = previous.text.len().saturating_sub(2);
                    Spot {
                        page: here.page - 1,
                        offset: aloud::sentence_start(&previous.text, last),
                    }
                } else {
                    Spot {
                        page: here.page,
                        offset: 0,
                    }
                }
            }
        };
        self.follow = true;
        if !self.narrator.seek(target) {
            self.narrator.start(target);
        }
    }

    fn change_speed(&mut self, faster: bool) {
        let speed = aloud::step_speed(self.narrator.speed(), faster);
        if speed != self.narrator.speed() {
            self.narrator.set_speed(speed);
            (self.options.on_speed)(speed);
        }
        self.notice = Some(format!("speed {}", aloud::speed_label(speed)));
    }

    /// Feeds the narrator text, takes in its speech, and keeps the word being
    /// read in view. True when the screen should be drawn again.
    fn narrate(&mut self) -> bool {
        let pages = self.pages();
        // A run of empty pages is crossed a few at a time, between keys.
        for _ in 0..8 {
            let Some((from, budget)) = self.narrator.wants() else {
                break;
            };
            let cut = if from.page >= pages {
                Cut::End
            } else {
                let laid = self.laid(from.page);
                match aloud::cut(&laid.text, from.offset, budget) {
                    Some((start, end)) => Cut::Passage {
                        page: from.page,
                        start,
                        text: laid.text[start..end].to_vec(),
                    },
                    None => Cut::PageDone,
                }
            };
            self.narrator.give(cut);
        }
        let before = self.narrator.said();
        let mut changed = self.narrator.tick();
        if let Some(notice) = self.narrator.take_notice() {
            self.notice = Some(notice);
            changed = true;
        }
        if let Some(said) = self.narrator.said()
            && Some(said) != before
        {
            let y = self.screen_row(said.page, said.start);
            if y.is_some() {
                self.follow = true;
            }
            // Turn before the word reaches the very bottom, so the next line shows too.
            let near_bottom = |y: usize| self.view() > 6 && y + 2 >= self.view();
            if self.follow && y.is_none_or(near_bottom) {
                self.show(said.page, said.start);
            }
        }
        changed
    }

    // -----------------------------------------------------------------------
    // Bookmarks.

    fn bookmarked(&self, page: usize) -> bool {
        self.bookmarks.iter().any(|b| b.page == page)
    }

    /// The start of a page's text, for the bookmark list.
    fn page_label(&mut self, page: usize) -> String {
        let text: String = self.laid(page).text.iter().collect();
        let line = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let mut label: String = line.chars().take(BOOKMARK_LABEL_CHARS).collect();
        if line.chars().count() > BOOKMARK_LABEL_CHARS {
            label.push('…');
        }
        label
    }

    /// Bookmarks the page being read, or removes its bookmark.
    fn toggle_bookmark(&mut self) {
        if self.doc.page_count.is_none() {
            return;
        }
        let page = self.reading();
        if self.bookmarked(page) {
            self.bookmarks.retain(|b| b.page != page);
        } else {
            let label = self.page_label(page);
            let at = self.bookmarks.partition_point(|b| b.page < page);
            self.bookmarks.insert(at, Bookmark { page, label });
        }
        (self.options.on_bookmarks)(&self.bookmarks);
    }

    /// Moving through the bookmark list; enter turns to the picked page.
    fn bookmarks_key(&mut self, key: &KeyEvent, selected: usize) {
        let last = self.bookmarks.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('\'') => self.dialog = Dialog::None,
            KeyCode::Down | KeyCode::Char('j') => {
                self.dialog = Dialog::Bookmarks {
                    selected: (selected + 1).min(last),
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dialog = Dialog::Bookmarks {
                    selected: selected.saturating_sub(1),
                }
            }
            KeyCode::Enter => {
                self.dialog = Dialog::None;
                if let Some(page) = self.bookmarks.get(selected).map(|b| b.page) {
                    self.go_to_page(page, false);
                }
            }
            // A held key would remove one bookmark after another.
            KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace
                if key.kind == KeyEventKind::Press
                    && selected <= last
                    && !self.bookmarks.is_empty() =>
            {
                self.bookmarks.remove(selected);
                (self.options.on_bookmarks)(&self.bookmarks);
                self.dialog = Dialog::Bookmarks {
                    selected: selected.min(self.bookmarks.len().saturating_sub(1)),
                };
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Search.

    fn seeking(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|s| matches!(s.seek, Seek::Seeking { .. }))
    }

    /// Searches from `from`, reading pages for a while.
    fn seek(&mut self, query: Query, direction: Direction, from: TextPos) {
        self.search = Some(Search {
            query,
            seek: Seek::Seeking {
                direction,
                from,
                step: 0,
            },
        });
        self.advance_search(SEARCH_SLICE);
    }

    /// Reads pages for the next match until one is found, every page has
    /// been read, or `budget` runs out; the event loop carries on from there.
    fn advance_search(&mut self, budget: Duration) {
        let Some(search) = self.search.take() else {
            return;
        };
        let Seek::Seeking {
            direction,
            from,
            mut step,
        } = search.seek
        else {
            self.search = Some(search);
            return;
        };
        let started = Instant::now();
        let pages = self.pages();
        let seek = loop {
            if pages == 0 || step > pages {
                break Seek::Missing;
            }
            let page = direction.page(from.page, step, pages);
            let keep = |start: usize| match direction {
                Direction::Forward if step == 0 => start >= from.offset,
                Direction::Forward if step == pages => start < from.offset,
                Direction::Backward if step == 0 => start < from.offset,
                Direction::Backward if step == pages => start >= from.offset,
                _ => true,
            };
            let mut ranges = page_matches(&search.query, &self.laid(page))
                .into_iter()
                .filter(|&(start, _)| keep(start));
            let hit = match direction {
                Direction::Forward => ranges.next(),
                Direction::Backward => ranges.next_back(),
            };
            if let Some((start, end)) = hit {
                break Seek::Found(Match { page, start, end });
            }
            step += 1;
            if started.elapsed() >= budget {
                break Seek::Seeking {
                    direction,
                    from,
                    step,
                };
            }
        };
        if let Seek::Found(found) = seek {
            self.reveal(found);
        }
        self.search = Some(Search { seek, ..search });
    }

    /// The match after (or before) the one the reader is on.
    fn next_match(&mut self, direction: Direction) {
        let Some(Search {
            query,
            seek: Seek::Found(here),
        }) = &self.search
        else {
            return;
        };
        let offset = match direction {
            Direction::Forward => here.start + 1,
            Direction::Backward => here.start,
        };
        self.seek(
            query.clone(),
            direction,
            TextPos {
                page: here.page,
                offset,
            },
        );
    }

    /// Turns to a match's page and, if it is below the first screen, scrolls
    /// it a third of the way down.
    fn reveal(&mut self, found: Match) {
        self.go_to_page(found.page, false);
        let laid = self.laid(found.page);
        let holds = |seg: &render::Seg| {
            seg.offset
                .is_some_and(|o| o <= found.start && found.start < o + seg.text.chars().count())
        };
        let row = laid
            .rows
            .iter()
            .position(|row| row.segs.iter().any(holds))
            .unwrap_or(0)
            + self.prefix(found.page);
        if row >= self.view() {
            let offset = row.saturating_sub(self.view() / 3);
            self.offset = match self.mode {
                Mode::Paged => offset.min(self.max_offset()),
                Mode::Scroll => offset,
            };
            if self.mode == Mode::Scroll {
                self.clamp_end();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Input.

    fn screen_step(&self) -> isize {
        self.view().saturating_sub(2).max(1) as isize
    }

    /// Typing in the go-to box: only digits that keep it a page in the book are taken.
    fn goto_key(&mut self, key: &KeyEvent, digits: String, page_count: usize) {
        match key.code {
            KeyCode::Esc => self.dialog = Dialog::None,
            KeyCode::Enter => {
                self.dialog = Dialog::None;
                if let Ok(page) = digits.parse::<usize>() {
                    self.go_to_page(page - 1, false);
                }
            }
            KeyCode::Backspace => {
                let mut digits = digits;
                digits.pop();
                self.dialog = Dialog::Goto { digits, page_count };
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let next = format!("{digits}{c}");
                if !next.starts_with('0') && next.parse::<usize>().is_ok_and(|n| n <= page_count) {
                    self.dialog = Dialog::Goto {
                        digits: next,
                        page_count,
                    };
                }
            }
            _ => {}
        }
    }

    /// `@` or ctrl+enter: ask an agent about the selection.
    fn open_ask(&mut self) {
        let passage = self.selected_text();
        if passage.trim().is_empty() {
            self.notice = Some("select some text, then ask".into());
            return;
        }
        self.dialog = Dialog::Ask {
            typed: String::new(),
            passage,
        };
        agent::report_status("blocked", "Question about the selection");
    }

    fn ask_key(&mut self, key: &KeyEvent, mut typed: String, passage: String) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.dialog = Dialog::None;
                agent::report_status("idle", "Reading");
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                typed.push('\n');
                self.dialog = Dialog::Ask { typed, passage };
            }
            KeyCode::Enter => {
                if typed.trim().is_empty() {
                    self.notice = Some("type a question".into());
                    self.dialog = Dialog::Ask { typed, passage };
                    return;
                }
                self.dialog = Dialog::None;
                self.submit_ask(&passage, &typed);
            }
            KeyCode::Backspace => {
                typed.pop();
                self.dialog = Dialog::Ask { typed, passage };
            }
            KeyCode::Char(c) if !ctrl => {
                typed.push(c);
                self.dialog = Dialog::Ask { typed, passage };
            }
            _ => {}
        }
    }

    /// Sends `passage`, the lines around it and `question` to the agent pane.
    /// When no agent takes it, the whole prompt is copied instead.
    fn submit_ask(&mut self, passage: &str, question: &str) {
        let context = self.selection_context(passage);
        agent::remember(passage, &context);
        let prompt = agent::with_question(&context, question);
        match self.send_ask(&prompt) {
            Ok(found) => {
                self.notice = Some(format!("sent to {}", found.command));
                agent::report_status("idle", &format!("Sent to {}", found.command));
            }
            Err(error) => {
                self.copy = Some(prompt);
                self.notice = Some(error.to_string());
                agent::report_status("error", "Couldn't reach the agent");
            }
        }
    }

    /// Document, page and the lines around `fallback`, which is the selection.
    fn selection_context(&mut self, fallback: &str) -> String {
        let mut excerpts: Vec<agent::Excerpt> = self
            .selected_ranges()
            .into_iter()
            .map(|range| agent::Excerpt {
                page: self.doc.page_count.map(|_| range.page + 1),
                text: self.laid(range.page).text.iter().collect(),
                start: range.start,
                end: range.end,
            })
            .collect();
        if excerpts.is_empty() && !fallback.trim().is_empty() {
            excerpts.push(agent::Excerpt {
                page: None,
                text: fallback.to_owned(),
                start: 0,
                end: fallback.chars().count(),
            });
        }
        agent::prompt(
            &self.doc.title,
            self.options.source.as_deref(),
            &excerpts,
            "",
        )
    }

    fn send_ask(&mut self, text: &str) -> Result<Agent, AgentFailed> {
        if let Some(deliver) = self.ask.as_mut() {
            return deliver(text);
        }
        agent::deliver(text)
    }

    /// Typing in the find box; enter searches from the page being read.
    fn find_key(&mut self, key: &KeyEvent, typed: String) {
        match key.code {
            KeyCode::Esc => self.dialog = Dialog::None,
            KeyCode::Enter => {
                self.dialog = Dialog::None;
                if let Some(query) = Query::new(&typed) {
                    self.seek(
                        query,
                        Direction::Forward,
                        TextPos {
                            page: self.current,
                            offset: 0,
                        },
                    );
                }
            }
            KeyCode::Backspace => {
                let mut typed = typed;
                typed.pop();
                self.dialog = Dialog::Find { typed };
            }
            KeyCode::Char(c) => {
                self.dialog = Dialog::Find {
                    typed: format!("{typed}{c}"),
                }
            }
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent, history: &mut ReaderHistory) {
        let place = (self.current, self.offset);
        self.act(key, history);
        // Scrolling away from the word being read stops the screen following it.
        if place != (self.current, self.offset) {
            self.follow = false;
        }
    }

    fn act(&mut self, key: KeyEvent, history: &mut ReaderHistory) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Before any dialog, which would otherwise take the keys as typing.
        if key.code == remote::WAKE {
            self.rex_command = Some(String::new());
            return;
        }
        if let Some(line) = self.rex_command.take() {
            return self.rex_key(&key, line);
        }
        if News::key(&mut self.news, key) {
            return;
        }
        match self.dialog.clone() {
            Dialog::Goto { digits, page_count } => return self.goto_key(&key, digits, page_count),
            Dialog::Find { typed } => return self.find_key(&key, typed),
            Dialog::Ask { typed, passage } => return self.ask_key(&key, typed, passage),
            Dialog::Bookmarks { selected } => return self.bookmarks_key(&key, selected),
            Dialog::Voices { typed, selected } => return self.voices_key(&key, typed, selected),
            Dialog::Help | Dialog::Guide => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Char('!')
                ) {
                    self.dialog = Dialog::None;
                }
                return;
            }
            Dialog::None => {}
        }
        self.notice = None;
        if self.tool == Tool::Move && self.arrange_key(&key) {
            return;
        }
        if let Some(hue) = typed(&key).and_then(Hue::of_key) {
            return self.pick_hue(hue);
        }
        let paged = self.mode == Mode::Paged;
        match typed(&key) {
            Some('?') | Some('/') => self.dialog = Dialog::Help,
            Some('!') => self.dialog = Dialog::Guide,
            Some('t') | Some(':') => {
                if let Some(page_count) = self.doc.page_count {
                    self.dialog = Dialog::Goto {
                        digits: String::new(),
                        page_count,
                    };
                }
            }
            Some('m') if key.kind == KeyEventKind::Press => self.toggle_bookmark(),
            Some('\'') if self.doc.page_count.is_some() => {
                // Opens on the bookmark at or before the page being read.
                let reading = self.reading();
                let selected = self
                    .bookmarks
                    .partition_point(|b| b.page <= reading)
                    .saturating_sub(1);
                self.dialog = Dialog::Bookmarks { selected };
            }
            Some('f') | Some('F') => {
                self.dialog = Dialog::Find {
                    typed: String::new(),
                }
            }
            Some('n') if self.found_here().is_some() => self.next_match(Direction::Forward),
            Some('N') => self.next_match(Direction::Backward),
            Some('q') => self.quit = true,
            Some('y') => self.toggle_highlight(),
            Some('@') => self.open_ask(),
            Some('j') => self.scroll_by(1),
            Some('k') => self.scroll_by(-1),
            // Plain d is the pen, so half a screen down is ctrl+d, as in vim.
            Some('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_by(self.screen_step() / 2)
            }
            Some('u') => self.scroll_by(-(self.screen_step() / 2)),
            Some(' ') => self.next_screen(paged),
            Some('b') => self.previous_screen(paged),
            Some('l') | Some('n') => self.turn(true),
            Some('h') | Some('p') => self.turn(false),
            Some('H') => self.revisit(history, Direction::Backward),
            Some('L') => self.revisit(history, Direction::Forward),
            Some('g') => self.go_to_page(0, false),
            Some('G') => self.go_to_page(self.last(), !paged),
            Some('i') => self.toggle_images(),
            Some('o') => self.toggle_look(),
            Some('d') | Some('D') => self.take_tool(Tool::Pen),
            Some('e') | Some('E') => self.take_tool(Tool::Eraser),
            Some('v') | Some('V') => self.take_tool(Tool::Move),
            Some('a') if key.kind == KeyEventKind::Press => self.read_aloud(),
            Some('A') if self.narrator.active() => self.narrator.stop(),
            Some('s') | Some('S') => self.open_voices(),
            Some('[') => self.change_speed(false),
            Some(']') => self.change_speed(true),
            // From the width the screen allows, so a column it narrowed answers at
            // once, and from the one asked for, so presses before the next frame add up.
            Some('<' | '-') => {
                let now = self.reading_width.within(self.room);
                self.reading_width =
                    ReadingWidth::Columns(now.saturating_sub(WIDTH_STEP).max(MIN_WIDTH))
            }
            Some('>' | '+') => {
                let now = self.reading_width.within(self.room);
                self.reading_width = ReadingWidth::Columns(now.saturating_add(WIDTH_STEP))
            }
            Some('=') => {
                self.reading_width = match self.reading_width {
                    ReadingWidth::Fit => ReadingWidth::Columns(self.options.max_text_width),
                    ReadingWidth::Columns(_) => ReadingWidth::Fit,
                }
            }
            Some(',') => self.skip_sentence(Direction::Backward),
            Some('.') => self.skip_sentence(Direction::Forward),
            _ => match key.code {
                KeyCode::Enter
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.kind == KeyEventKind::Press =>
                {
                    self.open_ask()
                }
                // Esc lets go of a picked stroke, puts the pen down, then lets
                // go of a selection, then a search, then quits.
                KeyCode::Esc if self.picked.take().is_some() => {}
                KeyCode::Esc if self.tool != Tool::Select => self.tool = Tool::Select,
                KeyCode::Esc => {
                    if self.selection.take().is_none() && self.search.take().is_none() {
                        if self.narrator.active() {
                            self.narrator.stop();
                        } else {
                            self.quit = true;
                        }
                    }
                }
                KeyCode::Down => self.scroll_by(1),
                KeyCode::Up => self.scroll_by(-1),
                KeyCode::PageDown => self.next_screen(paged),
                KeyCode::PageUp => self.previous_screen(paged),
                KeyCode::Right => self.turn(true),
                KeyCode::Left => self.turn(false),
                KeyCode::Home => self.go_to_page(0, false),
                KeyCode::End => self.go_to_page(self.last(), !paged),
                _ => {}
            },
        }
    }

    /// Paged: finish the page on screen, then turn it.
    fn next_screen(&mut self, paged: bool) {
        if paged && self.at_bottom() {
            if self.current < self.last() {
                self.go_to_page(self.current + 1, false);
            }
        } else {
            self.scroll_by(self.screen_step());
        }
    }

    /// Scrolls by a turn of the wheel. In paged mode the wheel runs on past a
    /// page's edge: down from its end turns to the next page, up from its top
    /// to the end of the previous one.
    fn wheel(&mut self, delta: isize) {
        if self.mode == Mode::Paged {
            if delta > 0 && self.at_bottom() && self.current < self.last() {
                return self.go_to_page(self.current + 1, false);
            }
            if delta < 0 && self.offset == 0 && self.current > 0 {
                return self.go_to_page(self.current - 1, true);
            }
        }
        self.scroll_by(delta);
    }

    fn previous_screen(&mut self, paged: bool) {
        if paged && self.offset == 0 && self.current > 0 {
            self.go_to_page(self.current - 1, true);
        } else {
            self.scroll_by(-self.screen_step());
        }
    }

    /// Switches an office document between its own design and terminal text,
    /// staying on the same page.
    fn toggle_look(&mut self) {
        let Some(look) = self.shown_as.map(crate::doc::Look::other) else {
            return;
        };
        // What was being read aloud is gone from the page, as on a reload.
        if self.narrator.active() {
            self.narrator.stop();
            self.notice = Some("the page changed, so reading aloud stopped".into());
        }
        self.shown_as = Some(look);
        self.doc.source.set_look(look);
        self.laid.clear();
        self.retain_protocols(|_| false);
        self.pixels.clear();
        self.selection = None;
        self.search = None;
        self.offset = 0;
    }

    fn toggle_images(&mut self) {
        self.image_mode = match self.image_mode {
            ImageMode::Color => ImageMode::Mono,
            ImageMode::Mono => ImageMode::Color,
        };
        (self.options.on_image_mode)(self.image_mode);
    }

    /// The link drawn in the screen cell at `x`, `y`, if any. Unlike `hit`,
    /// only a cell the link's own text covers counts.
    fn link_under(&mut self, x: u16, y: u16) -> Option<render::Link> {
        let row = y.checked_sub(self.column.y)?;
        let slot = (*self.screen.get(row as usize)?)?;
        let laid = self.laid(slot.page);
        let row = laid
            .rows
            .get(slot.row.checked_sub(self.prefix(slot.page))?)?;
        let col = x.checked_sub(self.column.x)?;
        for seg in &row.segs {
            let Some(offset) = seg.offset else { continue };
            let mut sx = seg.x;
            for (n, c) in seg.text.chars().enumerate() {
                let w = char_width(c).max(1);
                if (sx..sx + w).contains(&col) {
                    return laid.link_at(offset + n).cloned();
                }
                sx += w;
            }
        }
        None
    }

    /// Opens a web link in the browser or loads a local link in the reader.
    fn follow_link(&mut self, link: &render::Link, history: &mut ReaderHistory) {
        let doc = self
            .options
            .picture_path
            .as_deref()
            .filter(|path| path.is_file());
        match follow::resolve(link, doc) {
            Ok(Follow::Web(url)) => {
                self.notice = Some(match (self.open_url)(&url) {
                    Ok(()) => format!("opened {url}"),
                    Err(e) => format!("couldn't open {url} ({e})"),
                });
            }
            Ok(Follow::Open(path)) => match crate::load_file(&path) {
                Ok(doc) => {
                    if let Some(origin) = self.reading_location() {
                        ReaderHistory::push(&mut history.back, origin);
                    }
                    history.forward.clear();
                    self.go = Some(Go::Open {
                        path,
                        doc,
                        viewport: None,
                        notice: None,
                    });
                    self.quit = true;
                }
                Err(why) => self.notice = Some(why),
            },
            Err(why) => self.notice = Some(why),
        }
    }

    fn revisit(&mut self, history: &mut ReaderHistory, direction: Direction) {
        let (from, to) = match direction {
            Direction::Backward => (&mut history.back, &mut history.forward),
            Direction::Forward => (&mut history.forward, &mut history.back),
        };
        while let Some(ReadingLocation { path, viewport }) = from.pop_back() {
            let Some(Ok(doc)) = path.is_file().then(|| crate::load_file(&path)) else {
                self.notice = Some("skipped unavailable file in history".into());
                continue;
            };
            if let Some(origin) = self.reading_location() {
                ReaderHistory::push(to, origin);
            }
            self.go = Some(Go::Open {
                path,
                doc,
                viewport: Some(viewport),
                notice: self.notice.take(),
            });
            self.quit = true;
            break;
        }
    }

    /// Where in the text a screen cell falls: the character under it, or
    /// the nearest edge of the text on its row.
    fn hit(&mut self, x: u16, y: u16) -> Option<TextPos> {
        let y = y.clamp(self.column.y, self.column.bottom().saturating_sub(1));
        let slot = (*self.screen.get((y - self.column.y) as usize)?)?;
        let laid = self.laid(slot.page);
        let prefix = self.prefix(slot.page);
        let col = x.saturating_sub(self.column.x);
        let text_rows = laid
            .rows
            .iter()
            .enumerate()
            .skip(slot.row.saturating_sub(prefix));
        for (i, row) in text_rows {
            let segs: Vec<&render::Seg> = row.segs.iter().filter(|s| s.offset.is_some()).collect();
            let (Some(first), Some(last)) = (segs.first(), segs.last()) else {
                continue;
            };
            // A row below the one pointed at: its start.
            if i + prefix != slot.row || col < first.x {
                return first.offset.map(|offset| TextPos {
                    page: slot.page,
                    offset,
                });
            }
            for seg in &segs {
                let mut sx = seg.x;
                for (n, c) in seg.text.chars().enumerate() {
                    let w = char_width(c).max(1);
                    if col < sx + w {
                        return seg.offset.map(|o| TextPos {
                            page: slot.page,
                            offset: o + n,
                        });
                    }
                    sx += w;
                }
            }
            return last.offset.map(|o| TextPos {
                page: slot.page,
                offset: o + last.text.chars().count(),
            });
        }
        Some(TextPos {
            page: slot.page,
            offset: laid.text.len(),
        })
    }

    fn mouse(&mut self, mouse: MouseEvent, history: &mut ReaderHistory) {
        if News::mouse(&mut self.news, mouse) || self.dialog != Dialog::None {
            return;
        }
        let (x, y) = (mouse.column, mouse.row);
        match (mouse.kind, self.tool) {
            (MouseEventKind::Down(MouseButton::Left), Tool::Select) => {
                self.selection = self.hit(x, y).map(|at| Selection {
                    anchor: at,
                    head: at,
                });
                if self.selection.is_some() {
                    self.drag = Drag::Selecting;
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Tool::Pen) => {
                self.selection = None;
                if let Some(at) = self.hit(x, y)
                    && let Some(point) = self.point_from(at.page, at.offset, x, y)
                {
                    let serial = self.next_serial;
                    self.next_serial += 1;
                    self.drag = Drag::Inking(Inked {
                        serial,
                        stroke: Stroke::begin(at.page, at.offset, self.hue, point),
                    });
                }
            }
            (MouseEventKind::Down(MouseButton::Left), Tool::Eraser) => {
                self.selection = None;
                let erased = self.erase_at(x, y);
                self.drag = Drag::Erasing { erased };
            }
            (MouseEventKind::Down(MouseButton::Left), Tool::Move) => {
                self.selection = None;
                self.take_hold(x, y);
            }
            (MouseEventKind::Drag(MouseButton::Left), _) => match &self.drag {
                Drag::Idle => {}
                Drag::Selecting => {
                    if let (Some(selection), Some(head)) = (self.selection, self.hit(x, y)) {
                        self.selection = Some(Selection { head, ..selection });
                    }
                }
                Drag::Inking(inked) => {
                    let (page, anchor) = (inked.stroke.page, inked.stroke.anchor);
                    if let Some(point) = self.point_from(page, anchor, x, y)
                        && let Drag::Inking(inked) = &mut self.drag
                    {
                        inked.stroke.extend(point);
                    }
                }
                Drag::Erasing { erased } => {
                    let erased = *erased;
                    let now = self.erase_at(x, y);
                    self.drag = Drag::Erasing {
                        erased: erased || now,
                    };
                }
                Drag::Moving { from, original } => {
                    let (from, mut stroke) = (*from, original.clone());
                    stroke.shift(x as f32 - from.0 as f32, y as f32 - from.1 as f32);
                    self.replace_picked(stroke);
                }
                Drag::Resizing { corner, original } => {
                    let (corner, original) = (*corner, original.clone());
                    if let Some(p) = self.point_from(original.page, original.anchor, x, y) {
                        let to = original.extent().dragged(corner, handle_to_ink(corner, p));
                        self.replace_picked(original.fitted(to));
                    }
                }
            },
            (MouseEventKind::Up(MouseButton::Left), _) => {
                match std::mem::replace(&mut self.drag, Drag::Idle) {
                    Drag::Idle | Drag::Erasing { erased: false } => {}
                    Drag::Selecting => {
                        let text = self.selected_text();
                        if text.is_empty() {
                            self.selection = None;
                            // A click, not a drag: follow the link under it.
                            if let Some(link) = self.link_under(x, y) {
                                self.follow_link(&link, history);
                            }
                        } else {
                            // Taking over the mouse turns off the terminal's own selection, so copy here.
                            let context = self.selection_context(&text);
                            agent::remember(&text, &context);
                            self.copy = Some(text);
                        }
                    }
                    Drag::Inking(inked) => {
                        let font = self.picker.font_size();
                        let stroke = inked.stroke.tidied((font.width, font.height));
                        // A tidied stroke is drawn afresh, not from the images of it being drawn.
                        let serial = if stroke.trace == Trace::Shape {
                            self.next_serial += 1;
                            self.next_serial - 1
                        } else {
                            inked.serial
                        };
                        self.strokes.push(Inked { serial, stroke });
                        self.report_strokes();
                    }
                    Drag::Erasing { erased: true } => self.report_strokes(),
                    Drag::Moving { original, .. } | Drag::Resizing { original, .. } => {
                        if self.picked_stroke() != Some(&original) {
                            self.reanchor_picked();
                            self.report_strokes();
                        }
                    }
                }
            }
            (MouseEventKind::ScrollDown, _) => {
                self.follow = false;
                self.wheel(WHEEL_STEP)
            }
            (MouseEventKind::ScrollUp, _) => {
                self.follow = false;
                self.wheel(-WHEEL_STEP)
            }
            _ => {}
        }
    }

    /// The screen cell a page's character `anchor` is drawn in, if the page is on screen.
    fn anchor_cell(&mut self, page: usize, anchor: usize) -> Option<(i32, i32)> {
        let (y, slot) = self
            .screen
            .iter()
            .enumerate()
            .find_map(|(y, s)| s.filter(|s| s.page == page).map(|s| (y, s)))?;
        let top = self.column.y as i32 + y as i32 - slot.row as i32;
        let (row, col) = self.laid(page).locate(anchor);
        Some((
            self.column.x as i32 + col as i32,
            top + (self.prefix(page) + row) as i32,
        ))
    }

    /// The middle of screen cell `x`, `y`, measured from a page's character `anchor`.
    fn point_from(&mut self, page: usize, anchor: usize, x: u16, y: u16) -> Option<Point> {
        let (ox, oy) = self.anchor_cell(page, anchor)?;
        Some(Point {
            x: x as f32 + 0.5 - ox as f32,
            y: y as f32 + 0.5 - oy as f32,
        })
    }

    /// Rubs out every stroke on screen passing through cell `x`, `y`; says whether any went.
    fn erase_at(&mut self, x: u16, y: u16) -> bool {
        let pins: Vec<(usize, usize)> = self
            .strokes
            .iter()
            .map(|i| (i.stroke.page, i.stroke.anchor))
            .collect();
        let points: Vec<Option<Point>> = pins
            .into_iter()
            .map(|(page, anchor)| self.point_from(page, anchor, x, y))
            .collect();
        let before = self.strokes.len();
        let mut points = points.into_iter();
        self.strokes.retain(|inked| {
            !points
                .next()
                .flatten()
                .is_some_and(|p| inked.stroke.near(p, ERASER_REACH))
        });
        self.strokes.len() != before
    }

    fn picked_index(&self) -> Option<usize> {
        let serial = self.picked?;
        self.strokes.iter().position(|i| i.serial == serial)
    }

    fn picked_stroke(&self) -> Option<&Stroke> {
        self.picked_index().map(|i| &self.strokes[i].stroke)
    }

    /// Puts `stroke` in place of the picked one, under a new serial so it's drawn afresh.
    fn replace_picked(&mut self, stroke: Stroke) {
        let Some(i) = self.picked_index() else {
            return;
        };
        let serial = self.next_serial;
        self.next_serial += 1;
        self.strokes[i] = Inked { serial, stroke };
        self.picked = Some(serial);
    }

    /// With the move tool: a corner handle or the inside of the picked
    /// stroke's frame takes hold of it; otherwise the topmost stroke under
    /// the mouse is picked, ready to carry, or nothing is.
    fn take_hold(&mut self, x: u16, y: u16) {
        if let Some(stroke) = self.picked_stroke().cloned()
            && let Some(p) = self.point_from(stroke.page, stroke.anchor, x, y)
        {
            let frame = stroke.extent().padded(FRAME_PAD);
            let handle = Corner::ALL.into_iter().find(|&c| {
                let h = frame.corner(c);
                (p.x - h.x).abs() <= 1.0 && (p.y - h.y).abs() <= 0.75
            });
            if let Some(corner) = handle {
                self.drag = Drag::Resizing {
                    corner,
                    original: stroke,
                };
                return;
            }
            if frame.padded(Point { x: 0.5, y: 0.5 }).contains(p) {
                self.drag = Drag::Moving {
                    from: (x, y),
                    original: stroke,
                };
                return;
            }
        }
        self.picked = None;
        for i in (0..self.strokes.len()).rev() {
            let (page, anchor) = (self.strokes[i].stroke.page, self.strokes[i].stroke.anchor);
            if let Some(p) = self.point_from(page, anchor, x, y)
                && self.strokes[i].stroke.near(p, PICK_REACH)
            {
                self.picked = Some(self.strokes[i].serial);
                self.drag = Drag::Moving {
                    from: (x, y),
                    original: self.strokes[i].stroke.clone(),
                };
                return;
            }
        }
    }

    /// Pins the picked stroke to the word now under its first point, so a
    /// stroke carried to another paragraph moves with that one when text reflows.
    fn reanchor_picked(&mut self) {
        let Some(i) = self.picked_index() else {
            return;
        };
        let stroke = self.strokes[i].stroke.clone();
        let Some((ox, oy)) = self.anchor_cell(stroke.page, stroke.anchor) else {
            return;
        };
        let (sx, sy) = (
            ox as f32 + stroke.points[0].x,
            oy as f32 + stroke.points[0].y,
        );
        if sx < 0.0 || sy < 0.0 || sx > u16::MAX as f32 || sy > u16::MAX as f32 {
            return;
        }
        let Some(at) = self.hit(sx as u16, sy as u16) else {
            return;
        };
        let Some((nx, ny)) = self.anchor_cell(at.page, at.offset) else {
            return;
        };
        let mut moved = stroke;
        moved.shift((ox - nx) as f32, (oy - ny) as f32);
        moved.page = at.page;
        moved.anchor = at.offset;
        self.strokes[i].stroke = moved;
    }

    /// Keys for the picked stroke: arrows or `hjkl` nudge it a cell, `+`
    /// and `-` resize it, a number recolors it, and `x` deletes it. Says
    /// whether the key was one of them.
    fn arrange_key(&mut self, key: &KeyEvent) -> bool {
        let Some(stroke) = self.picked_stroke().cloned() else {
            return false;
        };
        let nudge = match (key.code, typed(key)) {
            (KeyCode::Left, _) | (_, Some('h')) => Some((-1.0, 0.0)),
            (KeyCode::Right, _) | (_, Some('l')) => Some((1.0, 0.0)),
            (KeyCode::Up, _) | (_, Some('k')) => Some((0.0, -1.0)),
            (KeyCode::Down, _) | (_, Some('j')) => Some((0.0, 1.0)),
            _ => None,
        };
        let changed = if let Some((dx, dy)) = nudge {
            let mut moved = stroke;
            moved.shift(dx, dy);
            moved
        } else if matches!(typed(key), Some('+') | Some('=')) {
            stroke.fitted(stroke.extent().scaled(GROW))
        } else if typed(key) == Some('-') {
            stroke.fitted(stroke.extent().scaled(1.0 / GROW))
        } else if let Some(hue) = typed(key).and_then(Hue::of_key) {
            self.hue = hue;
            Stroke { hue, ..stroke }
        } else if matches!(key.code, KeyCode::Delete | KeyCode::Backspace)
            || typed(key) == Some('x')
        {
            if let Some(i) = self.picked_index() {
                self.strokes.remove(i);
            }
            self.picked = None;
            self.report_strokes();
            return true;
        } else {
            return false;
        };
        self.replace_picked(changed);
        self.reanchor_picked();
        self.report_strokes();
        true
    }

    fn report_strokes(&mut self) {
        let strokes: Vec<Stroke> = self.strokes.iter().map(|i| i.stroke.clone()).collect();
        (self.options.on_strokes)(&strokes);
    }

    /// Why ink can't be drawn here, when it can't.
    fn no_ink(&self) -> Option<&'static str> {
        match (&self.kitty, self.picker.tmux_detected()) {
            (Some(_), _) => None,
            (None, true) => Some("drawing needs Kitty graphics, which leaf can't use inside tmux"),
            (None, false) => Some(
                "drawing needs Kitty graphics, which this terminal doesn't have; try Kitty, Ghostty or WezTerm",
            ),
        }
    }

    /// Takes up `tool`, or says why ink can't be drawn.
    fn take_tool(&mut self, tool: Tool) {
        if let Some(reason) = self.no_ink() {
            self.notice = Some(reason.to_owned());
            return;
        }
        self.selection = None;
        self.picked = None;
        self.tool = tool;
    }

    /// Draws the part of each stroke on screen, and the one being drawn.
    fn draw_strokes(&mut self, area: Rect) {
        if self.kitty.is_none() {
            return;
        }
        let pages: HashSet<usize> = self.screen.iter().flatten().map(|s| s.page).collect();
        let live = match &self.drag {
            Drag::Inking(inked) => Some(inked),
            _ => None,
        };
        let pins: HashSet<(usize, usize)> = self
            .strokes
            .iter()
            .chain(live)
            .map(|i| (i.stroke.page, i.stroke.anchor))
            .filter(|(page, _)| pages.contains(page))
            .collect();
        let origins: HashMap<(usize, usize), (i32, i32)> = pins
            .into_iter()
            .filter_map(|pin| Some((pin, self.anchor_cell(pin.0, pin.1)?)))
            .collect();
        let font = self.picker.font_size();
        let view = Cells {
            left: area.x as i32,
            top: self.column.y as i32,
            cols: area.width,
            rows: self.view_height,
        };
        let Some(kitty) = &mut self.kitty else {
            return;
        };
        let live = match &self.drag {
            Drag::Inking(inked) => Some(inked),
            _ => None,
        };
        let mut used = HashSet::new();
        for inked in self.strokes.iter().chain(live) {
            let stroke = &inked.stroke;
            let Some(&(ox, oy)) = origins.get(&(stroke.page, stroke.anchor)) else {
                continue;
            };
            let Some(visible) = stroke.bounds().shift(ox, oy).intersect(view) else {
                continue;
            };
            let key = InkKey {
                serial: inked.serial,
                points: stroke.points.len(),
                clip: visible.shift(-ox, -oy),
                frame: false,
            };
            let image = match self.ink_drawn.get(&key) {
                Some(image) => *image,
                None => {
                    let Some(image) =
                        stroke
                            .paint(key.clip, (font.width, font.height))
                            .and_then(|pixels| {
                                kitty.upload(&pixels, visible.cols, visible.rows, INK_LAYER)
                            })
                    else {
                        continue;
                    };
                    self.ink_drawn.insert(key, image);
                    image
                }
            };
            let rect = Rect {
                x: visible.left as u16,
                y: visible.top as u16,
                width: visible.cols,
                height: visible.rows,
            };
            self.placements.push((image, rect));
            used.insert(key);
        }
        // The picked stroke's frame, with a handle at each corner.
        let picked = self
            .picked
            .and_then(|serial| self.strokes.iter().find(|i| i.serial == serial));
        if let Some(inked) = picked
            && let Some(&(ox, oy)) = origins.get(&(inked.stroke.page, inked.stroke.anchor))
            && let Some(visible) = inked.stroke.frame_bounds().shift(ox, oy).intersect(view)
        {
            let key = InkKey {
                serial: inked.serial,
                points: inked.stroke.points.len(),
                clip: visible.shift(-ox, -oy),
                frame: true,
            };
            let image = match self.ink_drawn.get(&key) {
                Some(image) => Some(*image),
                None => {
                    stroke::paint_frame(inked.stroke.extent(), key.clip, (font.width, font.height))
                        .and_then(|pixels| {
                            kitty.upload(&pixels, visible.cols, visible.rows, FRAME_LAYER)
                        })
                        .inspect(|image| {
                            self.ink_drawn.insert(key, *image);
                        })
                }
            };
            if let Some(image) = image {
                let rect = Rect {
                    x: visible.left as u16,
                    y: visible.top as u16,
                    width: visible.cols,
                    height: visible.rows,
                };
                self.placements.push((image, rect));
                used.insert(key);
            }
        }
        self.ink_drawn.retain(|key, image| {
            let kept = used.contains(key);
            if !kept {
                kitty.free(*image);
            }
            kept
        });
    }

    /// The selection as ranges of page text, one per page it spans.
    fn selected_ranges(&mut self) -> Vec<Highlight> {
        let Some((start, end)) = self.selection.map(Selection::range) else {
            return Vec::new();
        };
        let mut ranges = Vec::new();
        for page in start.page..=end.page {
            let text = self.laid(page).text.clone();
            let from = if page == start.page { start.offset } else { 0 }.min(text.len());
            let to = if page == end.page {
                end.offset
            } else {
                text.len()
            }
            .min(text.len());
            if from < to {
                let slice: String = text[from..to].iter().collect();
                ranges.push(Highlight {
                    page,
                    start: from,
                    end: to,
                    text: slice.trim_end_matches('\n').to_owned(),
                    hue: self.hue,
                });
            }
        }
        ranges
    }

    fn selected_text(&mut self) -> String {
        self.selected_ranges()
            .into_iter()
            .map(|r| r.text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Highlights the selection, or clears highlights it touches.
    fn toggle_highlight(&mut self) {
        if self.selection.is_none() {
            return;
        }
        let ranges = highlight::merge(self.selected_ranges());
        self.highlights = highlight::toggle(&self.highlights, &ranges);
        self.selection = None;
        (self.options.on_highlights)(&self.highlights);
    }

    /// Makes `hue` the color of new highlights and ink. A selection is
    /// painted in it straight away: the highlights it touches change
    /// color, or it becomes one.
    fn pick_hue(&mut self, hue: Hue) {
        self.hue = hue;
        self.notice = Some(format!("{} picked", hue.name()));
        if self.selection.is_none() {
            return;
        }
        let ranges = highlight::merge(self.selected_ranges());
        self.highlights = highlight::paint(&self.highlights, &ranges, hue);
        self.selection = None;
        (self.options.on_highlights)(&self.highlights);
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        history: &mut ReaderHistory,
    ) -> io::Result<()> {
        // Set when the loop woke only to follow the narration.
        let mut quiet = false;
        loop {
            self.receive_images();
            if self.reload() {
                quiet = false;
            }
            self.advance_search(SEARCH_SLICE);
            let narrated = self.narrate();
            // Listening counts as reading.
            if narrated {
                self.clock.touch();
            }
            // Following the terminal's theme reads its reply from stdin, so it's
            // asked only after a wait that no key broke.
            let markdown_colors = self.markdown_colors();
            if self.look.refresh(quiet && !event::poll(Duration::ZERO)?) {
                if self.markdown_colors() != markdown_colors {
                    self.laid.clear();
                }
                quiet = false;
            }
            // Between words nothing on screen changes, so there's nothing to draw.
            if narrated || !quiet {
                terminal.draw(|f| self.draw(f))?;
                if let Some(kitty) = &mut self.kitty {
                    kitty.flush(&mut io::stdout(), &self.placements)?;
                }
            }
            if let Some(text) = self.copy.take() {
                let encoded = base64::engine::general_purpose::STANDARD.encode(text);
                let mut out = io::stdout();
                // A terminal that refuses the clipboard write only loses the copy.
                let _ = write!(out, "\x1b]52;c;{encoded}\x07").and_then(|_| out.flush());
            }
            self.report_page();
            if self.quit {
                if self.doc.page_count.is_none() {
                    self.report_scroll(true);
                }
                if let Some(kitty) = &mut self.kitty {
                    kitty.flush(&mut io::stdout(), &[])?;
                }
                return Ok(());
            }
            // A search keeps reading pages between keys, the word read aloud
            // is followed closely, and images downloading are checked for
            // often; otherwise sleep until something happens.
            let wait = if self.seeking() {
                Some(Duration::ZERO)
            } else if self.narrator.busy() {
                Some(NARRATION_TICK)
            } else {
                self.loading().then_some(Duration::from_millis(100))
            };
            // Wakes in time to follow the theme and the UI setting.
            let due = self.look.due();
            let due = match self.watch {
                Some(_) => due.min(WATCH_TICK),
                None => due,
            };
            let wait = Some(wait.map_or(due, |wait| wait.min(due)));
            quiet = false;
            if let Some(wait) = wait
                && !event::poll(wait)?
            {
                quiet = !self.seeking() && !self.loading();
                continue;
            }
            // Handle everything already waiting before drawing again.
            loop {
                self.clock.touch();
                match event::read()? {
                    Event::Key(key) => self.key(key, history),
                    Event::Mouse(mouse) => self.mouse(mouse, history),
                    Event::Resize(..) => self.resized(),
                    _ => {}
                }
                if self.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

/// Asks the terminal for its text (OSC 10) or background (OSC 11) color, so
/// painted images and diagrams match the text.
pub(crate) fn query_color(code: u8) -> Option<Rgb> {
    let reply = query_terminal(&format!("\x1b]{code};?\x1b\\"))?;
    parse_color_reply(&String::from_utf8_lossy(&reply))
}

/// Asks the terminal for one of its 256 palette colors (OSC 4).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn query_palette(index: u8) -> Option<Rgb> {
    let reply = query_terminal(&format!("\x1b]4;{index};?\x1b\\"))?;
    parse_color_reply(&String::from_utf8_lossy(&reply))
}

/// Asks the terminal whether it shows its light or dark theme (`CSI ? 996 n`,
/// which Rex and Ghostty answer); `Some(true)` for light.
pub(crate) fn query_color_scheme() -> Option<bool> {
    parse_scheme_reply(&query_terminal("\x1b[?996n")?)
}

/// Reads "ESC [ ? 997 ; 1 n" (dark) or "; 2 n" (light).
fn parse_scheme_reply(reply: &[u8]) -> Option<bool> {
    let at = reply.windows(5).position(|w| w == b"?997;")?;
    match reply.get(at + 5..at + 7)? {
        b"1n" => Some(false),
        b"2n" => Some(true),
        _ => None,
    }
}

/// Writes `request` and then a primary device attributes request (`CSI c`),
/// which every terminal answers, and in order: the read stops at that
/// answer, so a terminal that ignores `request` costs one round trip, not
/// the whole half-second deadline. Returns what came before the answer.
/// Reads a byte at a time, so no key typed after it is taken.
fn query_terminal(request: &str) -> Option<Vec<u8>> {
    let mut out = io::stdout();
    out.write_all(request.as_bytes()).ok()?;
    out.write_all(b"\x1b[c").ok()?;
    out.flush().ok()?;
    let mut reply = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        let left = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as i32;
        let mut fd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for stdin, polled with a timeout.
        if unsafe { libc::poll(&mut fd, 1, left.max(1)) } <= 0 {
            break;
        }
        let mut byte = 0u8;
        // SAFETY: reads one byte into a stack variable.
        let n = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut byte).cast(), 1) };
        if n <= 0 {
            break;
        }
        reply.push(byte);
        if let Some(at) = device_attributes_at(&reply) {
            reply.truncate(at);
            return Some(reply);
        }
    }
    // No attributes answer: the terminal said nothing, or only part of a reply.
    (!reply.is_empty()).then_some(reply)
}

/// Where the reply ends in a device attributes answer, `ESC [ ? 6 2 ; 2 2 c`.
fn device_attributes_at(reply: &[u8]) -> Option<usize> {
    if !reply.ends_with(b"c") {
        return None;
    }
    let start = reply.windows(3).rposition(|w| w == b"\x1b[?")?;
    let params = &reply[start + 3..reply.len() - 1];
    params
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b';')
        .then_some(start)
}

/// Reads "rgb:RRRR/GGGG/BBBB" (one to four hex digits a channel) from an OSC
/// color reply; some terminals answer "rgba:" with an alpha channel after.
fn parse_color_reply(reply: &str) -> Option<Rgb> {
    let (_, rgb) = reply
        .split_once("rgb:")
        .or_else(|| reply.split_once("rgba:"))?;
    let channels: Vec<u8> = rgb
        .split(['/', '\x07', '\x1b'])
        .take(3)
        .map(|hex| {
            let value = u32::from_str_radix(hex, 16).ok()?;
            let max = (1u32 << (4 * hex.len() as u32)) - 1;
            Some((value * 255 / max.max(1)) as u8)
        })
        .collect::<Option<_>>()?;
    Some([*channels.first()?, *channels.get(1)?, *channels.get(2)?])
}

/// What the reader asks `main` to open next.
pub(crate) enum Go {
    /// The next or previous picture in the same folder as `path`, the picture
    /// that was open: `step` is 1 for the next, -1 for the previous.
    Neighbour { path: PathBuf, step: i32 },
    /// A loaded file reached through a link or the session's history.
    Open {
        path: PathBuf,
        doc: Doc,
        viewport: Option<Viewport>,
        notice: Option<String>,
    },
}

/// Runs the reader in the terminal until the reader quits, or asks for
/// another document.
pub(crate) fn run(
    doc: Doc,
    options: ViewerOptions,
    history: &mut ReaderHistory,
) -> io::Result<Option<Go>> {
    static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(io::stdout(), DisableMouseCapture);
            previous(info);
        }));
    });
    let mut terminal = ratatui::init();
    // The title names the terminal tab; on screen it would only take rows from the text.
    let setup = execute!(io::stdout(), EnableMouseCapture, SetTitle(&doc.title));
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    // Lets Rex's command palette find this reader; Rex drops it if leaf dies.
    remote::announce(remote::Kind::Document, &doc.title);
    let mut app = App::new(doc, options, picker);
    let result = setup.and_then(|_| app.event_loop(&mut terminal, history));
    remote::withdraw(remote::Kind::Document);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result.map(|()| app.go.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;

    struct Book(Vec<String>);

    impl crate::doc::Source for Book {
        fn page(&self, index: usize) -> Rc<str> {
            self.0[index].as_str().into()
        }
        fn image_size(&self, _: &str) -> Option<(u32, u32)> {
            None
        }
        fn decode(&self, _: &str, _: u32) -> Option<RgbaImage> {
            None
        }
    }

    fn book(pages: &[&str]) -> Doc {
        Doc {
            title: "Book".into(),
            page_count: Some(pages.len()),
            source: Box::new(Book(pages.iter().map(|p| p.to_string()).collect())),
        }
    }

    struct Screen {
        app: App,
        terminal: Terminal<TestBackend>,
        history: ReaderHistory,
    }

    impl Screen {
        fn new(doc: Doc, width: u16, height: u16, options: ViewerOptions) -> Self {
            let app = App::new(doc, options, Picker::halfblocks());
            let mut screen = Screen {
                app,
                terminal: Terminal::new(TestBackend::new(width, height)).expect("test terminal"),
                history: ReaderHistory::default(),
            };
            screen.draw();
            screen
        }

        fn draw(&mut self) -> String {
            let app = &mut self.app;
            self.terminal.draw(|f| app.draw(f)).expect("draws");
            app.report_page();
            let buffer = self.terminal.backend().buffer();
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

        fn press(&mut self, code: KeyCode) -> String {
            self.key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            self.draw()
        }

        fn key(&mut self, key: KeyEvent) {
            self.app.key(key, &mut self.history);
        }

        fn open_next(&mut self, options: ViewerOptions) -> String {
            assert!(
                self.app.quit,
                "the reader should leave the current document"
            );
            let Some(Go::Open {
                path,
                doc,
                viewport,
                notice,
            }) = self.app.go.take()
            else {
                panic!("the reader should have loaded a destination");
            };
            let options = ViewerOptions {
                watch: Some(Watch::live(path.clone())),
                picture_path: Some(path),
                start_viewport: viewport,
                start_notice: notice,
                ..options
            };
            self.app = App::new(doc, options, Picker::halfblocks());
            self.draw()
        }

        fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) -> String {
            self.app.mouse(
                MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
                &mut self.history,
            );
            self.draw()
        }

        /// Screen position of `text`.
        fn find(&mut self, text: &str) -> (u16, u16) {
            let frame = self.draw();
            frame
                .lines()
                .enumerate()
                .find_map(|(y, line)| {
                    line.find(text)
                        .map(|x| (line[..x].chars().count() as u16, y as u16))
                })
                .expect("text is on screen")
        }
    }

    /// A markdown file on disk, read in a screen whose browser records what it was asked to open.
    fn linked_note(name: &str, text: &str) -> (Screen, PathBuf, Rc<RefCell<Vec<String>>>) {
        let dir = std::env::temp_dir().join(format!("leaf-links-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("today.md");
        std::fs::write(&path, text).unwrap();
        std::fs::write(dir.join("plan.md"), "# Plan").unwrap();
        let options = ViewerOptions {
            picture_path: Some(path.clone()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(crate::markdown::open(&path).unwrap(), 80, 20, options);
        let opened = Rc::new(RefCell::new(Vec::new()));
        let log = opened.clone();
        s.app.open_url = Box::new(move |url| {
            log.borrow_mut().push(url.to_owned());
            Ok(())
        });
        (s, dir, opened)
    }

    fn click(s: &mut Screen, (x, y): (u16, u16)) {
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x, y);
    }

    fn click_text(s: &mut Screen, label: &str) {
        let position = s.find(label);
        click(s, position);
    }

    #[test]
    fn clicking_a_web_link_opens_it_in_the_browser() {
        let (mut s, dir, opened) = linked_note("web", "Read [the docs](https://x.dev/a) now.");
        let (x, y) = s.find("docs");
        click(&mut s, (x + 1, y));
        assert_eq!(*opened.borrow(), ["https://x.dev/a"]);
        assert!(!s.app.quit);
        // The URL shown after the label opens it too.
        let (x, y) = s.find("x.dev");
        click(&mut s, (x, y));
        assert_eq!(opened.borrow().len(), 2);
        // Plain text beside a link is just text.
        let (x, y) = s.find("Read");
        click(&mut s, (x, y));
        let (x, y) = s.find("now");
        click(&mut s, (x + 1, y));
        assert_eq!(opened.borrow().len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn clicking_a_label_works_with_the_url_hidden() {
        let options = ViewerOptions {
            hide_link_urls: true,
            ..ViewerOptions::default()
        };
        let url = "https://x.dev/a";
        let mut s = Screen::new(
            Doc::whole("Notes", format!("Read [the docs]({url}) now.")),
            80,
            20,
            options,
        );
        let opened = Rc::new(RefCell::new(Vec::new()));
        let log = opened.clone();
        s.app.open_url = Box::new(move |url| {
            log.borrow_mut().push(url.to_owned());
            Ok(())
        });
        let frame = s.draw();
        assert!(frame.contains("Read the docs now."));
        assert!(!frame.contains(url));
        click_text(&mut s, "docs");
        assert_eq!(*opened.borrow(), [url]);
    }

    #[test]
    fn clicking_a_wikilink_opens_its_note_in_leaf() {
        let (mut s, dir, opened) = linked_note("wiki", "Next: [[plan|the plan]] and [[nowhere]].");
        let (x, y) = s.find("nowhere");
        click(&mut s, (x, y));
        assert!(!s.app.quit);
        assert!(s.draw().contains("no note named nowhere"));
        let (x, y) = s.find("the plan");
        click(&mut s, (x + 2, y));
        assert!(matches!(&s.app.go, Some(Go::Open { path, .. }) if *path == dir.join("plan.md")));
        assert!(s.app.quit);
        assert!(opened.borrow().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_restores_the_exact_row_in_both_directions() {
        let filler = "A short paragraph.\n\n".repeat(40);
        let text = format!(
            "{}[[plan|Open Plan]]\n\n{filler}",
            "A short paragraph.\n\n".repeat(10)
        );
        let (mut s, dir, _) = linked_note("history-rows", &text);
        std::fs::write(dir.join("plan.md"), &filler).unwrap();
        // Row 12 is between paragraphs. Its next text anchor is on row 13.
        s.app.scroll_by(12);
        let origin = s.app.reading_location().unwrap();
        assert_eq!(origin.viewport.offset, 12);
        click_text(&mut s, "Open Plan");
        let anchor = origin
            .viewport
            .anchor
            .as_ref()
            .expect("text follows the blank row")
            .offset;
        assert_eq!(s.app.screen_row(origin.viewport.page, anchor), Some(1));
        assert_eq!(s.history.back.back(), Some(&origin));
        s.open_next(ViewerOptions::default());
        s.app.scroll_by(22);
        let destination = s.app.reading_location().unwrap();
        assert_eq!(destination.viewport.offset, 22);

        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions {
            start_anchor: Some(0),
            ..ViewerOptions::default()
        });
        assert_eq!(s.app.reading_location(), Some(origin));

        s.press(KeyCode::Char('L'));
        s.open_next(ViewerOptions::default());
        assert_eq!(s.app.reading_location(), Some(destination));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_finds_the_same_text_after_edits_above_it_while_away() {
        let filler = "A short paragraph.\n\n".repeat(40);
        let intro = "A paragraph before the saved position.\n\n".repeat(6);
        let text = format!("{intro}The saved passage. [[plan|Open Plan]]\n\n{filler}");
        let plan = format!("{intro}The saved line in the plan.\n\n{filler}");
        let (mut s, dir, _) = linked_note("history-edits", &text);
        let plan_path = dir.join("plan.md");
        std::fs::write(&plan_path, plan).unwrap();
        let (_, row) = s.find("The saved passage.");
        s.app.scroll_by(row as isize);
        assert_eq!(s.find("The saved passage.").1, 0);
        click_text(&mut s, "Open Plan");
        s.open_next(ViewerOptions::default());

        let (_, row) = s.find("The saved line in the plan.");
        s.app.scroll_by(row as isize);
        assert_eq!(s.find("The saved line in the plan.").1, 0);
        std::fs::write(
            dir.join("today.md"),
            format!("{}{}", "A new paragraph.\n\n".repeat(14), text),
        )
        .unwrap();

        s.press(KeyCode::Char('H'));
        let frame = s.open_next(ViewerOptions::default());
        assert!(frame.starts_with("  The saved passage."), "{frame}");

        std::fs::write(
            &plan_path,
            format!("The saved line in the plan.\n\n{filler}"),
        )
        .unwrap();
        s.press(KeyCode::Char('L'));
        let frame = s.open_next(ViewerOptions::default());
        assert!(
            frame.starts_with("  The saved line in the plan."),
            "{frame}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_reflows_text_when_the_terminal_is_resized_while_away() {
        let words = "one two three four five six seven eight nine ten ";
        let text = format!(
            "{} [[plan|Open Plan]] {}",
            words.repeat(20),
            words.repeat(60)
        );
        let (mut s, dir, _) = linked_note("history-reflow", &text);
        s.app.scroll_by(10);
        let before = s.app.reading_location().unwrap().viewport;
        let anchor = before.anchor.expect("visible text has an anchor").offset;
        click_text(&mut s, "Open Plan");
        s.open_next(ViewerOptions::default());

        s.terminal.backend_mut().resize(44, 20);
        s.draw();
        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions::default());
        assert_ne!(s.app.width, before.width);
        assert_ne!(s.app.offset, before.offset);
        assert_eq!(s.app.screen_row(before.page, anchor), Some(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_restores_an_office_page_and_look_across_reader_modes() {
        let (mut s, dir, _) = linked_note("history-office", "[Open sheet](sheet.csv)");
        let rows = (1..=150)
            .map(|row| format!("{row},Row {row}\n"))
            .collect::<String>();
        std::fs::write(dir.join("sheet.csv"), format!("Number,Name\n{rows}")).unwrap();
        s.terminal.backend_mut().resize(120, 30);
        click_text(&mut s, "Open sheet");
        s.open_next(ViewerOptions::default());
        s.press(KeyCode::Char('o'));
        s.press(KeyCode::Char('l'));
        s.app.scroll_by(8);
        let before = s.app.reading_location().unwrap().viewport;
        assert_eq!(before.mode, Mode::Paged);
        assert_eq!(before.page, 1);
        assert_eq!(before.look, Some(crate::doc::Look::Terminal));
        let anchor = before
            .anchor
            .expect("visible cells have a text anchor")
            .offset;

        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions::default());
        s.terminal.backend_mut().resize(120, 20);
        s.draw();
        s.press(KeyCode::Char('L'));
        s.open_next(ViewerOptions {
            start_page: Some(1),
            ..ViewerOptions::default()
        });
        assert_eq!(s.app.mode, Mode::Scroll);
        assert_eq!(s.app.width, before.width);
        assert_eq!(s.app.current, before.page);
        assert_eq!(s.app.shown_as, before.look);
        assert_eq!(s.app.screen_row(before.page, anchor), Some(0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_clamps_an_image_row_when_the_terminal_grows() {
        let (mut s, dir, _) = linked_note("history-image", "[Open image](tall.bmp)");
        RgbaImage::from_pixel(160, 640, image::Rgba([255, 0, 0, 255]))
            .save(dir.join("tall.bmp"))
            .unwrap();
        s.terminal.backend_mut().resize(80, 8);
        click_text(&mut s, "Open image");
        s.open_next(ViewerOptions::default());
        s.app.scroll_by(10);
        let picture = s.app.reading_location().unwrap();
        assert_eq!(picture.viewport.anchor, None);
        assert_eq!(picture.viewport.offset, 10);

        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions::default());
        s.terminal.backend_mut().resize(44, 50);
        s.draw();
        s.press(KeyCode::Char('L'));
        s.open_next(ViewerOptions::default());
        assert_eq!(s.app.options.picture_path.as_ref(), Some(&picture.path));
        assert_eq!(s.app.offset, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_skips_missing_files_and_shows_the_notice_where_it_lands() {
        let (mut s, dir, _) = linked_note("history-missing", "[[plan|Open Plan]]");
        std::fs::write(dir.join("plan.md"), "[[later|Read Later]]").unwrap();
        std::fs::write(dir.join("later.md"), "# Later").unwrap();
        click_text(&mut s, "Open Plan");
        s.open_next(ViewerOptions::default());
        click_text(&mut s, "Read Later");
        s.open_next(ViewerOptions::default());

        std::fs::remove_file(dir.join("plan.md")).unwrap();
        s.press(KeyCode::Char('H'));
        let frame = s.open_next(ViewerOptions::default());
        assert!(
            frame.contains("skipped unavailable file in history"),
            "{frame}"
        );
        assert_eq!(s.app.doc.title, "today");
        assert_eq!(s.history.forward.len(), 1);

        std::fs::remove_file(dir.join("later.md")).unwrap();
        let frame = s.press(KeyCode::Char('L'));
        assert!(
            frame.contains("skipped unavailable file in history"),
            "{frame}"
        );
        assert!(!s.app.quit);
        assert!(s.app.go.is_none());
        assert!(s.history.forward.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_a_successful_new_link_clears_forward_history() {
        let links = "[[plan|Open Plan]] [Open broken](broken.txt) [[other|Open Other]]";
        let (mut s, dir, _) = linked_note("history-branch", links);
        std::fs::write(dir.join("broken.txt"), "unsupported format").unwrap();
        std::fs::write(dir.join("other.md"), "# Other").unwrap();
        click_text(&mut s, "Open Plan");
        s.open_next(ViewerOptions::default());
        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions::default());

        click_text(&mut s, "Open broken");
        assert!(!s.app.quit);
        assert!(s.app.go.is_none());
        assert!(s.draw().contains("Can't open"));
        assert!(s.history.back.is_empty());
        assert_eq!(s.history.forward.len(), 1);

        click_text(&mut s, "Open Other");
        s.open_next(ViewerOptions::default());
        assert_eq!(s.app.doc.title, "other");
        assert_eq!(s.history.back.len(), 1);
        assert!(s.history.forward.is_empty());
        s.press(KeyCode::Char('L'));
        assert!(!s.app.quit);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_stacks_keep_the_latest_hundred_distinct_adjacent_positions() {
        let mut history = ReaderHistory::default();
        let at = |offset| ReadingLocation {
            path: std::env::temp_dir().join("note.md"),
            viewport: Viewport {
                page: 0,
                offset,
                anchor: None,
                width: 76,
                mode: Mode::Scroll,
                look: None,
            },
        };
        for stack in [&mut history.back, &mut history.forward] {
            for offset in 0..102 {
                ReaderHistory::push(stack, at(offset));
                ReaderHistory::push(stack, at(offset));
            }
            assert_eq!(stack.len(), 100);
            assert_eq!(stack.front(), Some(&at(2)));
            assert_eq!(stack.back(), Some(&at(101)));
        }
    }

    #[test]
    fn history_keys_leave_dialogs_and_prompts_in_control_after_reload() {
        let (mut s, dir, _) = linked_note("history-prompts", "[[plan|Open Plan]]");
        let plan = dir.join("plan.md");
        std::fs::write(&plan, "[[later|Read Later]]").unwrap();
        std::fs::write(dir.join("later.md"), "# Later").unwrap();
        click_text(&mut s, "Open Plan");
        s.open_next(ViewerOptions::default());
        click_text(&mut s, "Read Later");
        s.open_next(ViewerOptions::default());
        s.press(KeyCode::Char('H'));
        s.open_next(ViewerOptions::default());

        std::fs::write(&plan, "Updated Plan").unwrap();
        std::thread::sleep(crate::doc::LIVE_TICK);
        assert!(s.app.reload());
        assert!(s.draw().contains("Updated Plan"));
        assert_eq!(s.history.back.len(), 1);
        assert_eq!(s.history.forward.len(), 1);

        let help = s.press(KeyCode::Char('?'));
        assert!(help.contains("back / forward through followed links"));
        for key in ['H', 'L'] {
            s.press(KeyCode::Char(key));
        }
        assert_eq!(s.app.dialog, Dialog::Help);
        s.press(KeyCode::Esc);

        s.press(KeyCode::Char('f'));
        for key in ['H', 'L'] {
            s.press(KeyCode::Char(key));
        }
        assert_eq!(s.app.dialog, Dialog::Find { typed: "HL".into() });
        s.press(KeyCode::Esc);

        s.press(remote::WAKE);
        for key in ['H', 'L'] {
            s.press(KeyCode::Char(key));
        }
        assert_eq!(s.app.rex_command.as_deref(), Some("HL"));
        assert!(!s.app.quit);
        assert!(s.app.go.is_none());
        assert_eq!(s.history.back.len(), 1);
        assert_eq!(s.history.forward.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dragging_across_a_link_selects_instead_of_following_it() {
        let (mut s, dir, opened) = linked_note("drag", "Read [the docs](https://x.dev/a) now.");
        let (x, y) = s.find("the docs");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 5, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y);
        assert!(opened.borrow().is_empty());
        assert_eq!(s.app.copy.as_deref(), Some("the d"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn headings_follow_the_reader_theme() {
        for (colors, expected) in crate::scheme::HEADING_COLOR_CASES {
            let doc = book(&["## Heading"]);
            let mut app = App::new(doc, ViewerOptions::default(), Picker::halfblocks());
            app.look.set_test_colors(colors);
            let mut screen = Screen {
                app,
                terminal: Terminal::new(TestBackend::new(80, 20)).expect("test terminal"),
                history: ReaderHistory::default(),
            };
            let position = screen.find("Heading");
            assert_eq!(screen.terminal.backend().buffer()[position].fg, expected);
        }
    }

    #[test]
    fn reader_can_color_heading_levels_and_link_labels() {
        let md = "# Top heading that wraps in a narrow column\n\
                  ## Second\n### Third\n###### Sixth\n\n\
                  Body [**docs**](guide.md) and [[notes|Wiki]].\n\n\
                  > [Quoted](#anchor) and `snippet`";
        for enabled in [false, true] {
            let mut screen = Screen::new(
                book(&[md]),
                40,
                30,
                ViewerOptions {
                    colorful_markdown: enabled,
                    ..ViewerOptions::default()
                },
            );
            for (text, default, colorful) in [
                ("Top", Color::LightBlue, Color::LightMagenta),
                ("Second", Color::LightBlue, Color::LightBlue),
                ("Third", Color::LightBlue, Color::LightGreen),
                ("Sixth", Color::LightBlue, Color::LightGreen),
                ("Body", Color::Reset, Color::Reset),
                ("docs", Color::Reset, Color::LightCyan),
                ("Wiki", Color::Reset, Color::LightCyan),
                ("guide.md", render::DIM, render::DIM),
                ("Quoted", render::DIM, Color::LightCyan),
                ("snippet", render::DIM, render::DIM),
            ] {
                let position = screen.find(text);
                let cell = &screen.terminal.backend().buffer()[position];
                assert_eq!(cell.fg, if enabled { colorful } else { default }, "{text}");
                if matches!(text, "docs" | "Wiki" | "Quoted") {
                    assert!(cell.modifier.contains(Modifier::UNDERLINED), "{text}");
                }
            }
        }
    }

    /// A page with one figure that never decodes, counting the tries.
    struct Broken(Rc<std::cell::Cell<usize>>);

    impl crate::doc::Source for Broken {
        fn page(&self, _: usize) -> Rc<str> {
            "Before\n\n![](leaf-image:mask)\n\nAfter".into()
        }
        fn image_size(&self, _: &str) -> Option<(u32, u32)> {
            Some((160, 80))
        }
        fn decode(&self, _: &str, _: u32) -> Option<RgbaImage> {
            self.0.set(self.0.get() + 1);
            None
        }
    }

    #[test]
    fn a_live_picture_uses_its_own_loader() {
        let path =
            std::env::temp_dir().join(format!("leaf-picture-live-{}.png", std::process::id()));
        RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let options = ViewerOptions {
            watch: Some(Watch::new(path.clone())),
            ..ViewerOptions::default()
        };
        let mut screen = Screen::new(
            crate::load(path.to_str().unwrap()).unwrap(),
            80,
            20,
            options,
        );
        assert_eq!(screen.app.doc.source.image_size("0"), Some((2, 2)));
        RgbaImage::from_pixel(8, 4, image::Rgba([0, 255, 0, 255]))
            .save(&path)
            .unwrap();
        std::thread::sleep(WATCH_TICK);
        assert!(!screen.app.reload());
        std::thread::sleep(WATCH_TICK);
        assert!(screen.app.reload());
        assert_eq!(screen.app.doc.source.image_size("0"), Some((8, 4)));
        screen.draw();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn review_fix_cached_frames_do_not_reread_page_text() {
        struct Counted {
            text: Rc<str>,
            reads: Rc<std::cell::Cell<usize>>,
        }

        impl crate::doc::Source for Counted {
            fn page(&self, _: usize) -> Rc<str> {
                self.reads.set(self.reads.get() + 1);
                self.text.clone()
            }

            fn image_size(&self, _: &str) -> Option<(u32, u32)> {
                None
            }

            fn decode(&self, _: &str, _: u32) -> Option<RgbaImage> {
                None
            }
        }

        for text in ["", "A cached page\n\nAnother paragraph."] {
            let reads = Rc::new(std::cell::Cell::new(0));
            let doc = Doc {
                title: "Counted book".into(),
                page_count: Some(1),
                source: Box::new(Counted {
                    text: text.into(),
                    reads: reads.clone(),
                }),
            };
            let mut screen = Screen::new(doc, 120, 30, ViewerOptions::default());
            let frame = screen.draw();
            assert!(frame.contains(if text.is_empty() {
                "blank page"
            } else {
                "A cached page"
            }));
            assert_eq!(reads.get(), 1, "cached frames reread the source page");
            screen.draw();
            assert_eq!(reads.get(), 1);
            screen.app.laid.clear();
            screen.draw();
            assert_eq!(reads.get(), 2, "invalidated layout must reread once");
        }
    }

    #[test]
    fn review_fix_progress_reports_changed_page_count_without_navigation() {
        let pages = Rc::new(RefCell::new(Vec::new()));
        let heard = pages.clone();
        let options = ViewerOptions {
            on_page: Box::new(move |page| heard.borrow_mut().push(page)),
            ..ViewerOptions::default()
        };
        let mut screen = Screen::new(book(&["Alpha"]), 120, 30, options);
        assert_eq!(pages.borrow().len(), 1);
        screen.app.doc = book(&["Alpha", "Bravo"]);
        screen.app.laid.clear();
        screen.draw();
        assert_eq!(
            pages.borrow().len(),
            2,
            "replacement count was not reported"
        );
        assert_eq!(
            *pages.borrow(),
            vec![Progress::of(1, 1), Progress::of(1, 2)]
        );
        screen.press(KeyCode::Right);
        assert_eq!(pages.borrow().last(), Some(&Progress::of(2, 2)));
        screen.draw();
        assert_eq!(
            pages.borrow().len(),
            3,
            "unchanged progress was reported twice"
        );
    }

    /// A page with one figure that decodes to a solid square.
    struct Pictured;

    impl crate::doc::Source for Pictured {
        fn page(&self, _: usize) -> Rc<str> {
            "Before\n\n![](leaf-image:square)\n\nAfter".into()
        }
        fn image_size(&self, _: &str) -> Option<(u32, u32)> {
            Some((160, 80))
        }
        fn decode(&self, _: &str, _: u32) -> Option<RgbaImage> {
            Some(RgbaImage::from_pixel(
                160,
                80,
                image::Rgba([0, 0, 255, 255]),
            ))
        }
    }

    fn figure_ids(s: &Screen) -> Vec<u32> {
        s.app
            .placements
            .iter()
            .filter(|(image, _)| image.layer == 0)
            .map(|(image, _)| image.id)
            .collect()
    }

    #[test]
    fn a_resize_uploads_figures_again_because_terminals_may_drop_them() {
        let doc = Doc {
            title: "Book".into(),
            page_count: Some(1),
            source: Box::new(Pictured),
        };
        let mut s = Screen::new(doc, 120, 30, ViewerOptions::default());
        s.app.kitty = Some(Kitty::default());
        // The first frame drew it in half blocks, before kitty was there.
        s.app.protocols.clear();
        s.draw();
        let before = figure_ids(&s);
        assert_eq!(before.len(), 1);
        // The column stays 76 wide, so nothing else would make the figure change.
        s.app.resized();
        s.terminal.backend_mut().resize(118, 30);
        s.draw();
        let after = figure_ids(&s);
        assert_eq!(after.len(), 1);
        assert_ne!(
            before, after,
            "the figure kept an id the terminal may have dropped"
        );
    }

    #[test]
    fn a_resize_uploads_ink_again_too() {
        let (mut s, _, _) = with_a_line();
        let ink = |s: &Screen| -> Vec<u32> {
            s.app
                .placements
                .iter()
                .filter(|(image, _)| image.layer == INK_LAYER)
                .map(|(image, _)| image.id)
                .collect()
        };
        let before = ink(&s);
        assert_eq!(before.len(), 1);
        s.app.resized();
        s.draw();
        let after = ink(&s);
        assert_eq!(after.len(), 1);
        assert_ne!(
            before, after,
            "the ink kept an id the terminal may have dropped"
        );
    }

    #[test]
    fn width_keys_pressed_before_the_next_frame_all_count() {
        let mut s = Screen::new(
            Doc::whole("Notes", "A".repeat(300)),
            160,
            30,
            ViewerOptions::default(),
        );
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        s.key(key(KeyCode::Char('<')));
        s.key(key(KeyCode::Char('<')));
        s.draw();
        assert_eq!(s.app.column.width, 76 - 2 * WIDTH_STEP);
        s.key(key(KeyCode::Char('>')));
        s.key(key(KeyCode::Char('>')));
        s.key(key(KeyCode::Char('>')));
        s.draw();
        assert_eq!(s.app.column.width, 76 + WIDTH_STEP);
    }

    #[test]
    fn angle_brackets_narrow_and_widen_the_column_and_equals_fits_the_screen() {
        let mut s = Screen::new(
            Doc::whole("Notes", "A".repeat(300)),
            160,
            30,
            ViewerOptions::default(),
        );
        assert_eq!(s.app.column.width, 76);
        s.press(KeyCode::Char('<'));
        assert_eq!(s.app.column.width, 76 - WIDTH_STEP);
        s.press(KeyCode::Char('>'));
        s.press(KeyCode::Char('>'));
        assert_eq!(s.app.column.width, 76 + WIDTH_STEP);
        s.press(KeyCode::Char('='));
        assert_eq!(s.app.column.width, 156);
        // Wider than the screen stays inside it, and narrowing starts from what's shown.
        s.press(KeyCode::Char('>'));
        assert_eq!(s.app.column.width, 156);
        s.press(KeyCode::Char('<'));
        assert_eq!(s.app.column.width, 156 - WIDTH_STEP);
        s.press(KeyCode::Char('='));
        assert_eq!(s.app.column.width, 156);
        // Fitting again goes back to the configured width.
        s.press(KeyCode::Char('='));
        assert_eq!(s.app.column.width, 76);
        for _ in 0..40 {
            s.press(KeyCode::Char('<'));
        }
        assert_eq!(s.app.column.width, MIN_WIDTH);
    }

    #[test]
    fn a_figure_that_cant_be_decoded_is_tried_once_while_on_screen() {
        let tries = Rc::new(std::cell::Cell::new(0));
        let doc = Doc {
            title: "Book".into(),
            page_count: Some(1),
            source: Box::new(Broken(tries.clone())),
        };
        let mut s = Screen::new(doc, 120, 30, ViewerOptions::default());
        s.draw();
        s.draw();
        assert!(s.draw().contains("After"));
        assert_eq!(tries.get(), 1);
    }

    #[test]
    fn a_wide_window_shows_one_page_at_a_time_and_right_turns_it() {
        let mut s = Screen::new(
            book(&["Alpha text", "Bravo text", "Charlie text"]),
            120,
            30,
            ViewerOptions::default(),
        );
        let frame = s.draw();
        assert!(
            frame.contains("Alpha text") && !frame.contains("Bravo text"),
            "{frame}"
        );
        assert!(frame.contains("1 / 3 · 33%"));
        let frame = s.press(KeyCode::Right);
        assert!(
            frame.contains("Bravo text") && frame.contains("2 / 3"),
            "{frame}"
        );
        let frame = s.press(KeyCode::Char('G'));
        assert!(frame.contains("Charlie text") && frame.contains("3 / 3 · 100%"));
    }

    #[test]
    fn a_narrow_window_scrolls_through_pages_with_numbers_between() {
        let mut s = Screen::new(
            book(&["Alpha text", "Bravo text"]),
            60,
            20,
            ViewerOptions::default(),
        );
        let frame = s.draw();
        assert!(
            frame.contains("Alpha text")
                && frame.contains("·  2  ·")
                && frame.contains("Bravo text"),
            "{frame}"
        );
    }

    #[test]
    fn configured_width_controls_wrapping_and_centering() {
        assert_eq!(ViewerOptions::default().max_text_width, 76);
        for (maximum, terminal_width, width, x) in [
            (150, 160, 150, 5),
            (40, 160, 40, 60),
            (150, 60, 56, 2),
            (1, 160, 1, 79),
            (65535, 160, 156, 2),
        ] {
            let mut s = Screen::new(
                Doc::whole("Notes", "A".repeat(200)),
                terminal_width,
                30,
                ViewerOptions {
                    max_text_width: maximum,
                    ..ViewerOptions::default()
                },
            );
            assert_eq!(s.app.column, Rect::new(x, 0, width, 27));
            assert!(s.draw().contains(&"A".repeat(width as usize)));
        }
    }

    #[test]
    fn wide_tables_and_code_stay_inside_the_reading_column() {
        let table = "T".repeat(90);
        let code = "C".repeat(100);
        let text = format!("| Header |\n| --- |\n| {table} |\n\n```\n{code}\n```");
        let mut s = Screen::new(
            Doc::whole("Notes", text),
            160,
            30,
            ViewerOptions {
                max_text_width: 150,
                ..ViewerOptions::default()
            },
        );
        let frame = s.draw();
        assert!(frame.contains(&table) && frame.contains(&code));
        let column = s.app.column;
        assert!(frame.lines().any(|line| {
            let line = line.trim();
            line.starts_with('┌') && line.ends_with('┐')
        }));
        for line in frame.lines().take(column.height as usize) {
            assert!(line.chars().take(column.x as usize).all(|c| c == ' '));
            assert!(line.chars().skip(column.right() as usize).all(|c| c == ' '));
        }
    }

    #[test]
    fn reading_resumes_on_the_saved_page_and_page_changes_are_reported() {
        let pages = Rc::new(RefCell::new(Vec::new()));
        let heard = pages.clone();
        let options = ViewerOptions {
            start_page: Some(2),
            on_page: Box::new(move |p| heard.borrow_mut().push(p)),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Alpha", "Bravo", "Charlie"]), 120, 30, options);
        assert!(s.draw().contains("Bravo"));
        s.press(KeyCode::Char('l'));
        assert_eq!(
            *pages.borrow(),
            vec![Progress::of(2, 3), Progress::of(3, 3)]
        );
    }

    #[test]
    fn a_document_without_pages_resumes_at_its_line_at_any_width() {
        let text: String = (1..=80).map(|i| format!("Line {i}\n\n")).collect();
        let places = Rc::new(RefCell::new(Vec::new()));
        let heard = places.clone();
        let options = ViewerOptions {
            on_scroll: Box::new(move |p| heard.borrow_mut().push(p)),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(Doc::whole("Notes", text.clone()), 120, 30, options);
        s.press(KeyCode::Char(' '));
        // The text alone: wide screens dither their margins with half blocks.
        let frame = s.draw().replace(['▀', '▄'], " ");
        let top = frame
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("a line shows")
            .trim()
            .to_owned();
        s.app.report_scroll(true);
        let place = *places.borrow().last().expect("a place was heard");
        assert!(place.percent > 0, "{place:?}");

        let options = ViewerOptions {
            start_anchor: Some(place.anchor),
            ..ViewerOptions::default()
        };
        let mut again = Screen::new(Doc::whole("Notes", text), 80, 30, options);
        let frame = again.draw();
        assert_eq!(
            frame.lines().find(|l| !l.trim().is_empty()).map(str::trim),
            Some(top.as_str()),
            "{frame}"
        );
    }

    #[test]
    fn resizing_keeps_the_source_anchor_when_wide_text_reflows() {
        let text = "one two three four five six seven eight nine ten ".repeat(300);
        for (paged, before, after, width) in [(false, 160, 44, 40), (true, 90, 80, 76)] {
            let doc = if paged {
                book(&["Before", &text, "After"])
            } else {
                Doc::whole("Notes", text.clone())
            };
            let mut s = Screen::new(
                doc,
                before,
                30,
                ViewerOptions {
                    max_text_width: 150,
                    start_page: Some(2),
                    ..ViewerOptions::default()
                },
            );
            assert_eq!(s.app.mode, Mode::Scroll);
            s.press(KeyCode::Char(' '));
            let anchor = s.app.screen_top();
            let rows = s.app.laid(anchor.page).rows.len();
            assert!(anchor.offset > 76);
            s.terminal.backend_mut().resize(after, 30);
            s.draw();
            assert_eq!(s.app.column.width, width);
            assert_eq!(s.app.current, anchor.page);
            assert!(s.app.laid(anchor.page).rows.len() > rows);
            assert_eq!(s.app.screen_row(anchor.page, anchor.offset), Some(0));
        }
    }

    #[test]
    fn space_finishes_a_long_page_before_turning_it() {
        let long: String = (1..=60).map(|i| format!("Line {i}\n\n")).collect();
        let mut s = Screen::new(
            book(&[&long, "Next page"]),
            120,
            30,
            ViewerOptions::default(),
        );
        let frame = s.press(KeyCode::Char(' '));
        assert!(
            !frame.contains("Next page") && frame.contains("1 / 2"),
            "{frame}"
        );
        let mut frame = frame;
        for _ in 0..10 {
            if frame.contains("Next page") {
                break;
            }
            frame = s.press(KeyCode::Char(' '));
        }
        assert!(frame.contains("Next page"), "{frame}");
        let frame = s.press(KeyCode::Char('b'));
        assert!(
            frame.contains("Line 60"),
            "b goes back to the end of the previous page: {frame}"
        );
    }

    #[test]
    fn the_wheel_scrolls_through_to_the_next_and_previous_page() {
        let long: String = (1..=60).map(|i| format!("Line {i}\n\n")).collect();
        let mut s = Screen::new(
            book(&[&long, "Next page"]),
            120,
            30,
            ViewerOptions::default(),
        );
        let mut frame = s.draw();
        for _ in 0..60 {
            if frame.contains("Next page") {
                break;
            }
            frame = s.mouse(MouseEventKind::ScrollDown, 60, 10);
        }
        assert!(
            frame.contains("Next page") && frame.contains("2 / 2"),
            "{frame}"
        );
        let frame = s.mouse(MouseEventKind::ScrollUp, 60, 10);
        assert!(
            frame.contains("Line 60") && frame.contains("1 / 2"),
            "the wheel goes back to the end of the previous page: {frame}"
        );
    }

    #[test]
    fn m_bookmarks_the_page_and_the_list_turns_back_to_it() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_bookmarks: Box::new(move |b| *heard.borrow_mut() = b.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Alpha", "Bravo text", "Charlie"]), 120, 30, options);
        s.draw();
        s.press(KeyCode::Char('l'));
        assert!(s.press(KeyCode::Char('m')).contains("bookmarked"));
        assert_eq!(
            *saved.borrow(),
            vec![Bookmark {
                page: 1,
                label: "Bravo text".into()
            }]
        );
        s.press(KeyCode::Char('G'));
        let list = s.press(KeyCode::Char('\''));
        assert!(
            list.contains("Bookmarks") && list.contains("Bravo text"),
            "{list}"
        );
        let frame = s.press(KeyCode::Enter);
        assert!(
            frame.contains("Bravo text") && frame.contains("2 / 3"),
            "{frame}"
        );
        s.press(KeyCode::Char('m'));
        assert!(saved.borrow().is_empty());
    }

    #[test]
    fn a_bookmark_is_removed_from_the_list_with_x() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            bookmarks: vec![
                Bookmark {
                    page: 0,
                    label: "Alpha".into(),
                },
                Bookmark {
                    page: 2,
                    label: "Charlie".into(),
                },
            ],
            on_bookmarks: Box::new(move |b| *heard.borrow_mut() = b.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Alpha", "Bravo", "Charlie"]), 120, 30, options);
        s.draw();
        s.press(KeyCode::Char('\''));
        s.press(KeyCode::Char('j'));
        let list = s.press(KeyCode::Char('x'));
        assert_eq!(
            *saved.borrow(),
            vec![Bookmark {
                page: 0,
                label: "Alpha".into()
            }]
        );
        assert!(!list.contains("Charlie"), "{list}");
    }

    fn picture_folder(names: &[&str], tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-nav-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255]))
                .save_with_format(dir.join(name), image::ImageFormat::Bmp)
                .unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn picture_screen(path: &Path) -> Screen {
        let options = ViewerOptions {
            source: Some(path.display().to_string()),
            picture_path: Some(path.to_path_buf()),
            ..ViewerOptions::default()
        };
        Screen::new(picture::open(path).unwrap(), 100, 30, options)
    }

    #[test]
    fn keys_on_a_picture_ask_for_its_neighbour_in_the_folder() {
        let dir = picture_folder(&["a.bmp", "b.bmp", "c.bmp"], "keys");
        let middle = dir.join("b.bmp");
        let go = |code: KeyCode| {
            let mut s = picture_screen(&middle);
            s.press(code);
            assert!(s.app.quit, "{code:?} should close the current picture");
            let Some(Go::Neighbour { path, step }) = s.app.go else {
                panic!("{code:?} should ask for a neighbouring picture");
            };
            assert_eq!(path, middle, "{code:?}");
            step
        };
        for code in [KeyCode::Right, KeyCode::Char('l'), KeyCode::Char('n')] {
            assert_eq!(go(code), 1, "{code:?}");
        }
        for code in [KeyCode::Left, KeyCode::Char('h'), KeyCode::Char('p')] {
            assert_eq!(go(code), -1, "{code:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_picture_shows_its_place_among_the_pictures_in_its_folder() {
        let dir = picture_folder(&["a.bmp", "b.bmp", "c.bmp"], "place");
        let mut s = picture_screen(&dir.join("b.bmp"));
        let frame = s.draw();
        assert!(frame.contains("2 / 3"), "{frame}");
        let help = s.press(KeyCode::Char('?'));
        assert!(help.contains("next picture in the folder"), "{help}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lone_picture_does_not_navigate() {
        let dir = picture_folder(&["only.bmp"], "lone");
        let mut s = picture_screen(&dir.join("only.bmp"));
        s.press(KeyCode::Right);
        assert!(s.app.go.is_none() && !s.app.quit);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn holding_x_removes_only_one_bookmark() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            bookmarks: vec![
                Bookmark {
                    page: 0,
                    label: "Alpha".into(),
                },
                Bookmark {
                    page: 2,
                    label: "Charlie".into(),
                },
            ],
            on_bookmarks: Box::new(move |b| *heard.borrow_mut() = b.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Alpha", "Bravo", "Charlie"]), 120, 30, options);
        s.draw();
        s.press(KeyCode::Char('\''));
        s.press(KeyCode::Char('x'));
        s.key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: KeyEventState::NONE,
        });
        assert_eq!(
            *saved.borrow(),
            vec![Bookmark {
                page: 2,
                label: "Charlie".into()
            }]
        );
    }

    #[test]
    fn the_key_list_shows_on_question_mark_and_closes_without_quitting() {
        let mut s = Screen::new(book(&["Alpha", "Bravo"]), 120, 30, ViewerOptions::default());
        assert!(!s.draw().contains("quit"));
        for close in [KeyCode::Esc, KeyCode::Char('?'), KeyCode::Char('q')] {
            let frame = s.press(KeyCode::Char('?'));
            assert!(
                frame.contains("Keys") && frame.contains("go to page") && frame.contains("quit"),
                "{frame}"
            );
            assert!(!s.press(close).contains("go to page"));
            assert!(!s.app.quit);
        }
    }

    #[test]
    fn go_to_page_takes_only_digits_within_the_book() {
        let mut s = Screen::new(
            book(&[
                "A1", "B2", "C3", "D4", "E5", "F6", "G7", "H8", "I9", "J10", "K11", "L12",
            ]),
            120,
            30,
            ViewerOptions::default(),
        );
        s.press(KeyCode::Char('t'));
        s.press(KeyCode::Char('1'));
        s.press(KeyCode::Char('3'));
        let frame = s.press(KeyCode::Char('2'));
        assert!(frame.contains("12▏ / 12"), "{frame}");
        let frame = s.press(KeyCode::Enter);
        assert!(
            frame.contains("L12") && frame.contains("12 / 12"),
            "{frame}"
        );
    }

    #[test]
    fn dragging_selects_text_copies_it_and_y_highlights_it() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_highlights: Box::new(move |h| *heard.borrow_mut() = h.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Some words to select here."]), 120, 30, options);
        let (x, y) = s.find("words");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 5, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y);
        assert_eq!(s.app.copy.as_deref(), Some("words"));
        s.press(KeyCode::Char('y'));
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0].text, "words");
        let cell = &s.terminal.backend().buffer()[(x, y)];
        assert_eq!(cell.bg, Hue::Yellow.marker());
        // Selecting inside it and pressing y again clears it.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 1, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        s.press(KeyCode::Char('y'));
        assert!(saved.borrow().is_empty());
    }

    #[test]
    fn a_number_key_paints_the_selection_and_recolors_a_highlight() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_highlights: Box::new(move |h| *heard.borrow_mut() = h.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Some words to select here."]), 120, 30, options);
        let (x, y) = s.find("words");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 5, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y);
        let frame = s.press(KeyCode::Char('3'));
        assert!(frame.contains("blue picked"));
        assert_eq!(s.terminal.backend().buffer()[(x, y)].bg, Hue::Blue.marker());
        // A selection inside it turns it pink, and new highlights stay pink.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 1, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 2, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 2, y);
        s.press(KeyCode::Char('4'));
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0].hue, Hue::Pink);
        assert_eq!(
            s.terminal.backend().buffer()[(x + 4, y)].bg,
            Hue::Pink.marker()
        );
        assert_eq!(s.app.hue, Hue::Pink);
    }

    /// A screen whose terminal takes kitty images, so ink can be drawn.
    fn inking(text: &str, options: ViewerOptions) -> Screen {
        let mut s = Screen::new(book(&[text]), 120, 30, options);
        s.app.kitty = Some(Kitty::default());
        s.draw();
        s
    }

    fn ink_on_screen(s: &Screen) -> Vec<Rect> {
        s.app
            .placements
            .iter()
            .filter(|(image, _)| image.layer == INK_LAYER)
            .map(|(_, rect)| *rect)
            .collect()
    }

    #[test]
    fn drawing_without_kitty_graphics_says_why_and_stays_selecting() {
        let mut s = Screen::new(book(&["Some words"]), 120, 30, ViewerOptions::default());
        let frame = s.press(KeyCode::Char('D'));
        assert!(frame.contains("drawing needs Kitty graphics"));
        assert_eq!(s.app.tool, Tool::Select);
    }

    #[test]
    fn saved_ink_without_kitty_graphics_says_it_is_hidden() {
        let options = ViewerOptions {
            strokes: vec![Stroke::begin(0, 0, Hue::Red, Point { x: 0.5, y: 0.5 })],
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&["Some words"]), 120, 30, options);
        assert!(s.draw().contains("drawings here need Kitty graphics"));
    }

    #[test]
    fn a_stroke_shows_while_drawn_is_saved_and_follows_its_word_when_text_reflows() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_strokes: Box::new(move |s| *heard.borrow_mut() = s.to_vec()),
            ..ViewerOptions::default()
        };
        let text = format!("{}target here.", "word ".repeat(12));
        let mut s = inking(&text, options);
        s.press(KeyCode::Char('D'));
        s.press(KeyCode::Char('3'));
        let (x, y) = s.find("target");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 5, y);
        assert_eq!(ink_on_screen(&s).len(), 1, "ink shows before letting go");
        assert!(saved.borrow().is_empty());
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y);
        assert_eq!(saved.borrow().len(), 1);
        assert_eq!(saved.borrow()[0].hue, Hue::Blue);
        assert_eq!(saved.borrow()[0].points.len(), 2);
        let ink = ink_on_screen(&s)[0];
        let from_word =
            |rect: Rect, (x, y): (u16, u16)| (rect.x as i32 - x as i32, rect.y as i32 - y as i32);
        let before = from_word(ink, (x, y));

        // A narrower window wraps the word onto the next line; the ink goes with it.
        s.terminal.backend_mut().resize(60, 30);
        let moved = s.find("target");
        assert_ne!(moved, (x, y));
        assert_eq!(from_word(ink_on_screen(&s)[0], moved), before);
        assert!(s.draw().contains("pen blue"));
    }

    #[test]
    fn ink_on_a_document_without_pages_scrolls_with_its_line() {
        let text: String = (1..=80).map(|i| format!("Line {i}\n\n")).collect();
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_strokes: Box::new(move |s| *heard.borrow_mut() = s.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(Doc::whole("Notes", text), 120, 30, options);
        s.app.kitty = Some(Kitty::default());
        s.press(KeyCode::Char('D'));
        let (x, y) = s.find("Line 9");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y);
        assert_eq!(saved.borrow()[0].page, 0);
        let before = ink_on_screen(&s)[0];
        s.press(KeyCode::Char('j'));
        s.press(KeyCode::Char('j'));
        let (mx, my) = s.find("Line 9");
        assert_eq!(my, y - 2);
        let after = ink_on_screen(&s)[0];
        assert_eq!((after.x, after.y), (before.x + mx - x, before.y - 2));
    }

    #[test]
    fn the_eraser_rubs_out_a_stroke_it_passes_over() {
        let saved = Rc::new(RefCell::new(vec![Stroke::begin(
            0,
            0,
            Hue::Red,
            Point { x: 0.5, y: 0.5 },
        )]));
        let heard = saved.clone();
        let options = ViewerOptions {
            strokes: saved.borrow().clone(),
            on_strokes: Box::new(move |s| *heard.borrow_mut() = s.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = inking("Some words", options);
        assert_eq!(ink_on_screen(&s).len(), 1, "saved ink comes back");
        let (x, y) = s.find("Some");
        s.press(KeyCode::Char('e'));
        // Passing by a cell away leaves it.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        assert_eq!(saved.borrow().len(), 1);
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x, y);
        assert!(saved.borrow().is_empty());
        assert!(ink_on_screen(&s).is_empty());
    }

    #[test]
    fn a_box_drawn_by_hand_is_saved_as_a_rectangle() {
        let saved = Rc::new(RefCell::new(Vec::new()));
        let heard = saved.clone();
        let options = ViewerOptions {
            on_strokes: Box::new(move |s| *heard.borrow_mut() = s.to_vec()),
            ..ViewerOptions::default()
        };
        let mut s = inking(&"Some words to draw around. ".repeat(20), options);
        s.press(KeyCode::Char('d'));
        let (x, y) = s.find("Some");
        let mut path: Vec<(u16, u16)> = (0..=14).map(|i| (x + i, y)).collect();
        path.extend((1..=5).map(|i| (x + 14, y + i)));
        path.extend((0..14).rev().map(|i| (x + i, y + 5)));
        path.extend((1..5).rev().map(|i| (x, y + i)));
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        for &(px, py) in &path[1..] {
            s.mouse(MouseEventKind::Drag(MouseButton::Left), px, py);
        }
        s.mouse(MouseEventKind::Up(MouseButton::Left), x, y + 1);
        let strokes = saved.borrow();
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].trace, Trace::Shape);
        assert_eq!(strokes[0].points.len(), 5);
        assert_eq!(ink_on_screen(&s).len(), 1);
    }

    /// A screen with a level line of ink under the start of "Some words",
    /// six cells long, and the saved strokes as they're reported.
    fn with_a_line() -> (Screen, Rc<RefCell<Vec<Stroke>>>, (u16, u16)) {
        let line = Stroke {
            points: vec![Point { x: 0.5, y: 0.5 }, Point { x: 6.5, y: 0.5 }],
            trace: Trace::Shape,
            ..Stroke::begin(0, 0, Hue::Red, Point { x: 0.5, y: 0.5 })
        };
        let saved = Rc::new(RefCell::new(vec![line.clone()]));
        let heard = saved.clone();
        let options = ViewerOptions {
            strokes: vec![line],
            on_strokes: Box::new(move |s| *heard.borrow_mut() = s.to_vec()),
            ..ViewerOptions::default()
        };
        let text = "Some words here.\n\nAnother paragraph below it.\n\nAnd a third one.";
        let mut s = inking(text, options);
        let at = s.find("Some");
        (s, saved, at)
    }

    fn frames_on_screen(s: &Screen) -> usize {
        s.app
            .placements
            .iter()
            .filter(|(image, _)| image.layer == FRAME_LAYER)
            .count()
    }

    #[test]
    fn the_move_tool_picks_a_stroke_and_carries_it() {
        let (mut s, saved, (x, y)) = with_a_line();
        s.press(KeyCode::Char('v'));
        assert_eq!(s.app.tool, Tool::Move);
        // A click away from any ink picks nothing.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y + 4);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y + 4);
        assert_eq!(frames_on_screen(&s), 0);
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y);
        assert_eq!(frames_on_screen(&s), 1, "a picked stroke shows its frame");
        let before = ink_on_screen(&s)[0];
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 5, y + 2);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 5, y + 2);
        let after = ink_on_screen(&s)[0];
        assert_eq!((after.x, after.y), (before.x + 2, before.y + 2));
        assert_eq!(saved.borrow().len(), 1);
        // Carried onto another paragraph, it's pinned to the text there now.
        assert_ne!(saved.borrow()[0].anchor, 0);
        let extent = saved.borrow()[0].extent();
        assert_eq!(extent.width(), 6.0);
    }

    #[test]
    fn dragging_a_corner_handle_resizes_the_picked_stroke() {
        let (mut s, saved, (x, y)) = with_a_line();
        s.press(KeyCode::Char('v'));
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        // The bottom-right handle sits just past the line's right end.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 6, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 12, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 12, y);
        let extent = saved.borrow()[0].extent();
        assert!(extent.width() > 10.0, "{extent:?}");
        assert_eq!(extent.height(), 0.0, "a level line stays level");
    }

    #[test]
    fn keys_nudge_resize_recolor_and_delete_the_picked_stroke() {
        let (mut s, saved, (x, y)) = with_a_line();
        s.press(KeyCode::Char('v'));
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        let before = ink_on_screen(&s)[0];
        s.press(KeyCode::Right);
        assert_eq!(ink_on_screen(&s)[0].x, before.x + 1);
        s.press(KeyCode::Char('+'));
        assert!((saved.borrow()[0].extent().width() - 6.0 * GROW).abs() < 0.01);
        s.press(KeyCode::Char('3'));
        assert_eq!(saved.borrow()[0].hue, Hue::Blue);
        // Esc lets go of the stroke but keeps the move tool.
        s.press(KeyCode::Esc);
        assert_eq!(frames_on_screen(&s), 0);
        assert_eq!(s.app.tool, Tool::Move);
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 4, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y);
        s.press(KeyCode::Char('x'));
        assert!(saved.borrow().is_empty());
        assert!(ink_on_screen(&s).is_empty());
        assert!(!s.app.quit);
    }

    #[test]
    fn d_always_takes_the_pen_and_e_always_the_eraser() {
        let mut s = inking("Some words", ViewerOptions::default());
        for (key, tool) in [
            ('d', Tool::Pen),
            ('d', Tool::Pen),
            ('e', Tool::Eraser),
            ('E', Tool::Eraser),
            ('D', Tool::Pen),
        ] {
            s.press(KeyCode::Char(key));
            assert_eq!(s.app.tool, tool, "after {key}");
        }
    }

    #[test]
    fn ctrl_d_scrolls_half_a_screen_instead_of_taking_the_pen() {
        let text: String = (1..=80).map(|i| format!("Line {i}\n\n")).collect();
        let mut s = Screen::new(Doc::whole("Notes", text), 120, 30, ViewerOptions::default());
        s.app.kitty = Some(Kitty::default());
        s.key(KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(s.app.offset > 0);
        assert_eq!(s.app.tool, Tool::Select);
    }

    #[test]
    fn bang_opens_the_mark_up_guide_and_closes_it() {
        let mut s = Screen::new(book(&["Some words"]), 120, 30, ViewerOptions::default());
        let frame = s.press(KeyCode::Char('!'));
        assert!(frame.contains("Mark up"));
        assert!(frame.contains("1 yellow"));
        assert!(frame.contains("drag anywhere to draw"));
        assert!(frame.contains("Drawing needs Kitty graphics"));
        assert!(!s.press(KeyCode::Char('!')).contains("Mark up"));
        assert!(!s.app.quit);
    }

    #[test]
    fn escape_puts_the_pen_down_before_quitting() {
        let mut s = inking("Some words", ViewerOptions::default());
        s.press(KeyCode::Char('D'));
        assert_eq!(s.app.tool, Tool::Pen);
        s.press(KeyCode::Esc);
        assert_eq!(s.app.tool, Tool::Select);
        assert!(!s.app.quit);
        s.press(KeyCode::Esc);
        assert!(s.app.quit);
    }

    #[test]
    fn escape_clears_a_selection_before_quitting() {
        let mut s = Screen::new(book(&["Some words"]), 120, 30, ViewerOptions::default());
        let (x, y) = s.find("words");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);
        s.press(KeyCode::Esc);
        assert!(!s.app.quit && s.app.selection.is_none());
        s.press(KeyCode::Esc);
        assert!(s.app.quit);
    }

    fn search(s: &mut Screen, query: &str) -> String {
        s.press(KeyCode::Char('f'));
        for c in query.chars() {
            s.press(KeyCode::Char(c));
        }
        s.press(KeyCode::Enter)
    }

    #[test]
    fn find_turns_to_the_page_with_the_term_and_marks_it() {
        let mut s = Screen::new(
            book(&["Alpha text", "Bravo text", "Charlie text", "Delta text"]),
            120,
            30,
            ViewerOptions::default(),
        );
        let frame = search(&mut s, "charLIE");
        assert!(
            frame.contains("Charlie text") && frame.contains("3 / 4"),
            "{frame}"
        );
        assert!(frame.contains("\"charLIE\" n/N"), "{frame}");
        let (x, y) = s.find("Charlie");
        let buffer = s.terminal.backend().buffer();
        assert_eq!(buffer[(x, y)].bg, FOUND_HERE);
        assert_eq!(buffer[(x + 7, y)].bg, Color::Reset);
    }

    #[test]
    fn find_searches_the_label_and_visible_text_when_the_link_url_is_hidden() {
        let url = "https://x.dev/guide";
        let first = format!("Read [the guide]({url}) now.");
        let second = format!("The address is <{url}>.");
        let options = ViewerOptions {
            hide_link_urls: true,
            ..ViewerOptions::default()
        };
        let mut s = Screen::new(book(&[&first, &second]), 120, 30, options);

        assert!(search(&mut s, "guide now").contains("\"guide now\" n/N"));
        let (x, y) = s.find("guide now");
        let buffer = s.terminal.backend().buffer();
        assert_eq!(buffer[(x, y)].bg, FOUND_HERE);
        assert_eq!(buffer[(x + 8, y)].bg, FOUND_HERE);

        search(&mut s, url);
        assert_eq!(s.app.found_here().map(|m| m.page), Some(1));
        s.press(KeyCode::Char('N'));
        assert_eq!(s.app.found_here().map(|m| m.page), Some(1));

        let mut s = Screen::new(book(&[&first]), 120, 30, ViewerOptions::default());
        search(&mut s, url);
        assert_eq!(s.app.found_here().map(|m| m.page), Some(0));
    }

    #[test]
    fn a_match_far_down_a_page_is_scrolled_into_view() {
        let long: String = (1..=80)
            .map(|i| format!("Filler paragraph {i}.\n\n"))
            .collect::<String>()
            + "The needle is here.";
        let mut s = Screen::new(book(&[&long]), 120, 30, ViewerOptions::default());
        assert!(!s.draw().contains("needle"));
        assert!(search(&mut s, "needle").contains("The needle is here."));
    }

    #[test]
    fn n_and_shift_n_step_through_matches_wrapping_round() {
        let mut s = Screen::new(
            book(&["Alpha raft", "Bravo", "Raft one, raft two", "Delta"]),
            120,
            30,
            ViewerOptions::default(),
        );
        search(&mut s, "raft");
        let at = |s: &Screen| s.app.found_here().map(|m| (m.page, m.start));
        assert_eq!(at(&s), Some((0, 6)));
        s.press(KeyCode::Char('n'));
        assert_eq!(at(&s), Some((2, 0)));
        s.press(KeyCode::Char('n'));
        assert_eq!(at(&s), Some((2, 10)));
        s.press(KeyCode::Char('n'));
        assert_eq!(at(&s), Some((0, 6)));
        s.press(KeyCode::Char('N'));
        assert_eq!(at(&s), Some((2, 10)));
        assert!(s.draw().contains("3 / 4"));
    }

    #[test]
    fn a_term_nowhere_in_the_book_says_so_and_n_turns_pages_again() {
        let mut s = Screen::new(
            book(&["Alpha text", "Bravo text"]),
            120,
            30,
            ViewerOptions::default(),
        );
        let frame = search(&mut s, "zulu");
        assert!(
            frame.contains("1 / 2") && frame.contains("\"zulu\" not found"),
            "{frame}"
        );
        assert!(s.press(KeyCode::Char('n')).contains("Bravo text"));
    }

    #[test]
    fn escape_clears_a_search_before_quitting() {
        let mut s = Screen::new(book(&["Alpha text"]), 120, 30, ViewerOptions::default());
        search(&mut s, "alpha");
        let frame = s.press(KeyCode::Esc);
        assert!(!s.app.quit && !frame.contains("\"alpha\""), "{frame}");
        s.press(KeyCode::Esc);
        assert!(s.app.quit);
    }

    #[test]
    fn escape_in_the_find_box_closes_it_without_searching() {
        let mut s = Screen::new(
            book(&["Alpha text", "Bravo text"]),
            120,
            30,
            ViewerOptions::default(),
        );
        s.press(KeyCode::Char('f'));
        s.press(KeyCode::Char('b'));
        assert!(s.draw().contains("Find"));
        let frame = s.press(KeyCode::Esc);
        assert!(
            !frame.contains("Find") && frame.contains("Alpha text") && s.app.search.is_none(),
            "{frame}"
        );
    }

    #[test]
    fn asking_without_a_selection_says_to_select_first() {
        let mut s = Screen::new(book(&["Alpha text"]), 120, 30, ViewerOptions::default());
        let frame = s.press(KeyCode::Char('@'));
        assert!(frame.contains("select some text, then ask"), "{frame}");
    }

    #[test]
    fn a_question_about_a_selection_is_pasted_to_the_agent() {
        let sent = Rc::new(RefCell::new(None));
        let mut s = Screen::new(book(&["Alpha text"]), 120, 30, ViewerOptions::default());
        s.app.selection = Some(Selection {
            anchor: TextPos { page: 0, offset: 0 },
            head: TextPos {
                page: 0,
                offset: 200,
            },
        });
        let captured = sent.clone();
        s.app.ask = Some(Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(Agent {
                pane: "block:agent".into(),
                command: "claude".into(),
            })
        }));
        assert!(s.press(KeyCode::Char('@')).contains("Ask"));
        s.press(KeyCode::Char('w'));
        s.press(KeyCode::Char('h'));
        s.press(KeyCode::Char('y'));
        let frame = s.press(KeyCode::Enter);
        assert!(frame.contains("sent to claude"), "{frame}");
        let text = sent.borrow().clone().expect("sent");
        assert!(text.contains("From Book"), "{text}");
        assert!(text.contains("page 1"), "{text}");
        assert!(text.contains("Alpha"), "{text}");
        assert!(text.contains(">>"), "{text}");
        assert!(text.trim_end().ends_with("why"), "{text}");
    }

    #[test]
    fn asking_about_a_sentence_after_the_context_cap_sends_the_exact_selection() {
        let prefix = "Earlier text. ".repeat(40);
        let selected = "The selected sentence.";
        let paragraph = format!("{prefix}{selected} Following text.");
        let sent = Rc::new(RefCell::new(None));
        let mut s = Screen::new(
            Doc::whole("Notes", paragraph),
            120,
            30,
            ViewerOptions::default(),
        );
        let start = prefix.chars().count();
        s.app.selection = Some(Selection {
            anchor: TextPos {
                page: 0,
                offset: start,
            },
            head: TextPos {
                page: 0,
                offset: start + selected.chars().count(),
            },
        });
        let captured = sent.clone();
        s.app.ask = Some(Box::new(move |text| {
            *captured.borrow_mut() = Some(text.to_owned());
            Ok(Agent {
                pane: "block:agent".into(),
                command: "claude".into(),
            })
        }));
        let preview = s.press(KeyCode::Char('@'));
        assert!(preview.contains(selected), "{preview}");
        for c in "What does this sentence mean?".chars() {
            s.press(KeyCode::Char(c));
        }
        let frame = s.press(KeyCode::Enter);
        assert!(frame.contains("sent to claude"), "{frame}");
        let text = sent.borrow().clone().expect("sent");
        assert!(text.contains(selected), "{text}");
        assert!(
            text.contains(&format!("Selected:\n```\n{selected}\n```\n\nContext:\n")),
            "{text}"
        );
        assert!(text.contains("From Notes, line 1"), "{text}");
        assert!(
            text.trim_end().ends_with("What does this sentence mean?"),
            "{text}"
        );
    }

    #[test]
    fn centered_blocks_sit_in_the_middle_of_the_column() {
        let mut s = Screen::new(
            book(&["<div align=\"center\">\nSubmitted By\n</div>\n\nLeft text."]),
            120,
            30,
            ViewerOptions::default(),
        );
        let (centered, _) = s.find("Submitted By");
        let (left, _) = s.find("Left text.");
        assert_eq!(left, 22);
        assert_eq!(centered, 22 + (76 - 12) / 2);
    }

    #[test]
    fn a_document_without_pages_shows_scroll_percent() {
        let text: String = (1..=80).map(|i| format!("Line {i}\n\n")).collect();
        let mut s = Screen::new(
            Doc::whole("Article", text),
            80,
            20,
            ViewerOptions::default(),
        );
        assert!(s.draw().contains("0%"));
        let frame = s.press(KeyCode::Char('G'));
        assert!(
            frame.contains("100%") && frame.contains("Line 80"),
            "{frame}"
        );
    }

    fn aloud(options: ViewerOptions) -> ViewerOptions {
        ViewerOptions {
            speech: Backend::Silent(aloud::fake_synth()),
            ..options
        }
    }

    /// Lets the narrator take in speech until a word is being said, then
    /// plays `seconds` more of it.
    fn listen(s: &mut Screen, seconds: f32) -> String {
        for _ in 0..500 {
            s.app.narrate();
            if s.app.narrator.said().is_some() {
                break;
            }
            s.app.narrator.play_for(0.001);
            std::thread::sleep(Duration::from_millis(1));
        }
        s.app.narrator.play_for(seconds);
        s.app.narrate();
        s.draw()
    }

    /// The word marked as being read aloud.
    fn marked(s: &mut Screen) -> String {
        s.draw();
        let buffer = s.terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buffer[(x, y)].modifier.contains(Modifier::REVERSED))
            .map(|(x, y)| buffer[(x, y)].symbol().to_owned())
            .collect()
    }

    #[test]
    fn a_reads_aloud_from_the_top_of_the_screen_marking_each_word() {
        let mut s = Screen::new(
            book(&["Alpha beta gamma. Delta epsilon."]),
            80,
            20,
            aloud(ViewerOptions::default()),
        );
        s.press(KeyCode::Char('a'));
        let frame = listen(&mut s, 0.0);
        assert_eq!(marked(&mut s), "Alpha");
        assert!(frame.contains("▶ EmmaMultilingual 1×"), "{frame}");
        listen(&mut s, 1.2);
        assert_eq!(marked(&mut s), "beta");
        // Paused, the word stays put however long goes by.
        let frame = s.press(KeyCode::Char('a'));
        assert!(frame.contains("❚❚"), "{frame}");
        listen(&mut s, 5.0);
        assert_eq!(marked(&mut s), "beta");
        s.press(KeyCode::Char('a'));
        listen(&mut s, 1.0);
        assert_eq!(marked(&mut s), "gamma");
    }

    #[test]
    fn configured_width_does_not_change_speech_sentence_policy() {
        let text = format!(
            "{}Visible {}.",
            "Earlier ".repeat(18),
            "later ".repeat(1000)
        );
        let mut s = Screen::new(
            Doc::whole("Notes", text),
            160,
            30,
            ViewerOptions {
                max_text_width: 150,
                ..ViewerOptions::default()
            },
        );
        s.press(KeyCode::Down);
        s.press(KeyCode::Down);
        let offset = s.app.top_anchor();
        assert!(offset > 76 && offset < 150);
        assert_eq!(s.app.screen_top(), Spot { page: 0, offset });
    }

    #[test]
    fn reading_aloud_starts_at_the_selection() {
        let mut s = Screen::new(
            book(&["Alpha beta gamma. Delta epsilon."]),
            80,
            20,
            aloud(ViewerOptions::default()),
        );
        let (x, y) = s.find("Delta");
        s.mouse(MouseEventKind::Down(MouseButton::Left), x, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y);
        s.press(KeyCode::Char('a'));
        listen(&mut s, 0.0);
        assert_eq!(marked(&mut s), "Delta");
    }

    #[test]
    fn brackets_change_the_speed_and_it_is_kept() {
        let speeds = Rc::new(RefCell::new(Vec::new()));
        let heard = speeds.clone();
        let mut s = Screen::new(
            book(&["Alpha beta gamma delta epsilon zeta."]),
            80,
            20,
            aloud(ViewerOptions {
                on_speed: Box::new(move |speed| heard.borrow_mut().push(speed)),
                ..ViewerOptions::default()
            }),
        );
        s.press(KeyCode::Char('a'));
        listen(&mut s, 0.0);
        s.press(KeyCode::Char(']'));
        let frame = s.press(KeyCode::Char(']'));
        assert!(frame.contains("speed 1.5×"), "{frame}");
        // A second and a half of speech goes by in one second.
        listen(&mut s, 1.0);
        assert_eq!(marked(&mut s), "beta");
        listen(&mut s, 1.0);
        assert_eq!(marked(&mut s), "delta");
        s.press(KeyCode::Char('['));
        assert_eq!(*speeds.borrow(), vec![1.25, 1.5, 1.25]);
    }

    #[test]
    fn period_and_comma_skip_between_sentences() {
        let mut s = Screen::new(
            book(&["One two three. Four five six. Seven eight."]),
            80,
            20,
            aloud(ViewerOptions::default()),
        );
        s.press(KeyCode::Char('a'));
        listen(&mut s, 0.0);
        s.press(KeyCode::Char('.'));
        listen(&mut s, 0.0);
        assert_eq!(marked(&mut s), "Four");
        listen(&mut s, 2.2);
        assert_eq!(marked(&mut s), "six");
        // Well into a sentence, comma goes back to its start; at its start, to the one before.
        s.press(KeyCode::Char(','));
        listen(&mut s, 0.0);
        assert_eq!(marked(&mut s), "Four");
        s.press(KeyCode::Char(','));
        listen(&mut s, 0.0);
        assert_eq!(marked(&mut s), "One");
    }

    #[test]
    fn a_voice_from_the_rex_palette_is_used_even_with_a_dialog_open() {
        let voices = Rc::new(RefCell::new(Vec::new()));
        let heard = voices.clone();
        let mut s = Screen::new(
            book(&["Alpha beta."]),
            90,
            30,
            aloud(ViewerOptions {
                on_voice: Box::new(move |voice: &str| heard.borrow_mut().push(voice.to_owned())),
                ..ViewerOptions::default()
            }),
        );
        let command = |s: &mut Screen, line: &str| {
            s.press(remote::WAKE);
            for c in line.chars() {
                s.press(KeyCode::Char(c));
            }
            s.press(KeyCode::Enter)
        };
        s.press(KeyCode::Char('f'));
        let frame = command(&mut s, "voice en-GB-RyanNeural");
        assert!(frame.contains("Ryan reads next"), "{frame}");
        assert_eq!(*voices.borrow(), vec!["en-GB-RyanNeural".to_owned()]);
        command(&mut s, "play");
        let frame = listen(&mut s, 0.0);
        assert!(frame.contains("▶ Ryan"), "{frame}");
    }

    #[test]
    fn the_voice_picker_filters_and_picks_a_voice() {
        let voices = Rc::new(RefCell::new(Vec::new()));
        let heard = voices.clone();
        let mut s = Screen::new(
            book(&["Alpha beta."]),
            90,
            30,
            aloud(ViewerOptions {
                on_voice: Box::new(move |voice: &str| heard.borrow_mut().push(voice.to_owned())),
                ..ViewerOptions::default()
            }),
        );
        let frame = s.press(KeyCode::Char('s'));
        assert!(
            frame.contains("Voice") && frame.contains("Sonia"),
            "{frame}"
        );
        for c in "kingdom".chars() {
            s.press(KeyCode::Char(c));
        }
        let frame = s.press(KeyCode::Down);
        assert!(
            frame.contains("2 of 14") && !frame.contains("Aria"),
            "{frame}"
        );
        let frame = s.press(KeyCode::Enter);
        assert!(frame.contains("Ryan reads next"), "{frame}");
        assert_eq!(*voices.borrow(), vec!["en-GB-RyanNeural".to_owned()]);
        s.press(KeyCode::Char('a'));
        let frame = listen(&mut s, 0.0);
        assert!(frame.contains("▶ Ryan"), "{frame}");
    }

    #[test]
    fn the_voice_list_shows_only_once_every_voice_is_there() {
        let mut s = Screen::new(
            book(&["Alpha beta."]),
            90,
            30,
            aloud(ViewerOptions::default()),
        );
        let list = s.app.narrator.fetching_voices();
        let frame = s.press(KeyCode::Char('s'));
        assert!(frame.contains("Loading voices…"), "{frame}");
        assert!(
            !frame.contains("Sonia") && !frame.contains("Emma"),
            "{frame}"
        );
        // The search takes typing while the list is on its way; enter waits for it.
        for c in "hindi fem".chars() {
            s.press(KeyCode::Char(c));
        }
        let frame = s.press(KeyCode::Enter);
        assert!(frame.contains("search  hindi fem"), "{frame}");
        let voice = |name: &str, locale: &str, language: &str, gender: &str| edge::Voice {
            name: name.to_owned(),
            locale: locale.to_owned(),
            language: language.to_owned(),
            gender: gender.to_owned(),
        };
        list.send(Ok(vec![
            voice(
                "en-US-AriaNeural",
                "en-US",
                "English (United States)",
                "Female",
            ),
            voice("hi-IN-SwaraNeural", "hi-IN", "Hindi (India)", "Female"),
            voice("hi-IN-MadhurNeural", "hi-IN", "Hindi (India)", "Male"),
        ]))
        .expect("the list is listened for");
        s.app.narrate();
        let frame = s.draw();
        assert!(
            frame.contains("1 of 3") && frame.contains("Swara"),
            "{frame}"
        );
        assert!(
            !frame.contains("Madhur") && !frame.contains("Aria"),
            "{frame}"
        );
        let frame = s.press(KeyCode::Enter);
        assert!(frame.contains("Swara reads next"), "{frame}");
        assert_eq!(s.app.narrator.voice(), "hi-IN-SwaraNeural");
    }

    #[test]
    fn a_voice_picked_while_reading_takes_over_from_the_word_being_read() {
        let mut s = Screen::new(
            book(&["Alpha beta gamma delta."]),
            90,
            30,
            aloud(ViewerOptions::default()),
        );
        s.press(KeyCode::Char('a'));
        listen(&mut s, 1.2);
        assert_eq!(marked(&mut s), "beta");
        s.press(KeyCode::Char('s'));
        for c in "sonia".chars() {
            s.press(KeyCode::Char(c));
        }
        s.press(KeyCode::Enter);
        let frame = listen(&mut s, 0.0);
        assert!(frame.contains("▶ Sonia"), "{frame}");
        assert_eq!(marked(&mut s), "beta");
        listen(&mut s, 1.1);
        assert_eq!(marked(&mut s), "gamma");
    }

    #[test]
    fn the_screen_follows_the_word_read_aloud() {
        let text: String = (1..=60)
            .map(|i| format!("Sentence number {i}.\n\n"))
            .collect();
        let mut s = Screen::new(
            Doc::whole("Article", text),
            60,
            12,
            aloud(ViewerOptions::default()),
        );
        s.press(KeyCode::Char('a'));
        listen(&mut s, 0.0);
        for _ in 0..20 {
            s.press(KeyCode::Char('.'));
            listen(&mut s, 0.0);
        }
        let frame = s.draw();
        assert!(frame.contains("Sentence number 21."), "{frame}");
        assert_eq!(marked(&mut s), "Sentence");
    }

    #[test]
    fn escape_stops_reading_aloud_before_quitting() {
        let mut s = Screen::new(
            book(&["Alpha beta."]),
            80,
            20,
            aloud(ViewerOptions::default()),
        );
        s.press(KeyCode::Char('a'));
        listen(&mut s, 0.0);
        s.press(KeyCode::Esc);
        assert!(!s.app.narrator.active() && !s.app.quit);
        assert_eq!(marked(&mut s), "");
        s.press(KeyCode::Esc);
        assert!(s.app.quit);
    }

    #[test]
    fn osc_color_replies_parse_in_any_digit_count() {
        assert_eq!(
            parse_color_reply("\x1b]10;rgb:ffff/8080/0000\x1b\\"),
            Some([255, 128, 0])
        );
        assert_eq!(
            parse_color_reply("\x1b]10;rgb:ff/00/80\x07"),
            Some([255, 0, 128])
        );
        assert_eq!(
            parse_color_reply("\x1b]11;rgba:ffff/ffff/ffff/ffff\x1b\\"),
            Some([255, 255, 255])
        );
        assert_eq!(parse_color_reply("garbage"), None);
    }

    #[test]
    fn a_query_reply_ends_at_the_device_attributes_answer() {
        let rex = b"\x1b[?997;2n\x1b[?62;22c";
        assert_eq!(device_attributes_at(rex), Some(9));
        assert_eq!(parse_scheme_reply(&rex[..9]), Some(true));
        // A terminal that ignores the query answers only the attributes.
        assert_eq!(device_attributes_at(b"\x1b[?1;2c"), Some(0));
        assert_eq!(device_attributes_at(b"\x1b[?997;1n"), None);
        assert_eq!(
            device_attributes_at(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
            None
        );
    }

    #[test]
    fn color_scheme_replies_say_light_or_dark() {
        assert_eq!(parse_scheme_reply(b"\x1b[?997;1n"), Some(false));
        assert_eq!(parse_scheme_reply(b"\x1b[?997;2n"), Some(true));
        assert_eq!(parse_scheme_reply(b"\x1b[?997;3n"), None);
        assert_eq!(parse_scheme_reply(b"\x1b[?62;22c"), None);
    }
}
