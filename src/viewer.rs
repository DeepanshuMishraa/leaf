//! The reader: pages on screen, keys, mouse selection, highlights and figures.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
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

use crate::article;
use crate::doc::{Doc, ImageRef};
use crate::find::Query;
use crate::highlight::{self, Highlight};
use crate::kitty::{self, Kitty};
use crate::mono::{self, Rgb};
use crate::progress::{Bookmark, ImageMode, Progress, Scroll};
use crate::render::{self, DIM, Laid};

/// Widest the text column gets; a book-like measure is easiest to read.
const MAX_TEXT_WIDTH: u16 = 76;
/// Windows at least this big show one page per screen; smaller ones scroll.
const PAGED_MIN_WIDTH: u16 = 100;
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
/// How long a search reads pages before the screen is drawn and keys are read again.
const SEARCH_SLICE: Duration = Duration::from_millis(30);

const SELECTION: Color = Color::Indexed(8);
// Highlighter yellow under dark ink reads the same on light and dark themes.
const MARK: Color = Color::Rgb(250, 214, 64);
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

/// What floats over the text: nothing, the key list, the go-to-page box,
/// the find box with what's typed so far, or the bookmark list with the one picked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Dialog {
    None,
    Help,
    Goto { digits: String, page_count: usize },
    Find { typed: String },
    Bookmarks { selected: usize },
}

/// Which way a search moves through the pages.
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
}

/// What a row on screen shows.
#[derive(Debug, Clone, Copy)]
struct ScreenRow {
    page: usize,
    row: usize,
}

/// Hears the full set of highlights after each change.
pub type HighlightsListener = Box<dyn FnMut(&[Highlight])>;
pub type BookmarksListener = Box<dyn FnMut(&[Bookmark])>;

pub struct ViewerOptions {
    /// For paged documents, where reading resumes (1-based).
    pub start_page: Option<usize>,
    pub image_mode: ImageMode,
    pub highlights: Vec<Highlight>,
    /// Hears each page the reader settles on (1-based).
    pub on_page: Box<dyn FnMut(usize)>,
    /// For documents without pages, where reading resumes: a place in the page text.
    pub start_anchor: Option<usize>,
    /// Hears the place in a document without pages as each percent goes by, and on quitting.
    pub on_scroll: Box<dyn FnMut(Scroll)>,
    pub on_image_mode: Box<dyn FnMut(ImageMode)>,
    /// Hears every highlight there is, each time one is added or cleared.
    pub on_highlights: HighlightsListener,
    /// For paged documents, the pages marked to come back to, in page order.
    pub bookmarks: Vec<Bookmark>,
    /// Hears every bookmark there is, each time one is added or removed.
    pub on_bookmarks: BookmarksListener,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        ViewerOptions {
            start_page: None,
            image_mode: ImageMode::Color,
            highlights: Vec::new(),
            on_page: Box::new(|_| {}),
            start_anchor: None,
            on_scroll: Box::new(|_| {}),
            on_image_mode: Box::new(|_| {}),
            on_highlights: Box::new(|_| {}),
            bookmarks: Vec::new(),
            on_bookmarks: Box::new(|_| {}),
        }
    }
}

pub struct App {
    doc: Doc,
    options: ViewerOptions,
    picker: Picker,
    /// Column width pages are laid out at.
    width: u16,
    laid: HashMap<usize, Rc<Laid>>,
    mode: Mode,
    /// The page being read: in paged mode the one shown, when scrolling the one at the top.
    current: usize,
    /// Rows of the current page scrolled past.
    offset: usize,
    /// Rows the text area shows.
    view_height: u16,
    image_mode: ImageMode,
    ink: Option<Rgb>,
    paper: Option<Rgb>,
    highlights: Vec<Highlight>,
    bookmarks: Vec<Bookmark>,
    selection: Option<Selection>,
    dragging: bool,
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
    reported: Option<usize>,
    /// A place in a page-less document to scroll to at the next draw, once its layout is known.
    anchor: Option<usize>,
    reported_scroll: Option<Scroll>,
    copy: Option<String>,
    search: Option<Search>,
    quit: bool,
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

impl App {
    pub fn new(doc: Doc, options: ViewerOptions, picker: Picker) -> Self {
        let current = match (doc.page_count, options.start_page) {
            (Some(count), Some(start)) => start.clamp(1, count.max(1)) - 1,
            _ => 0,
        };
        let (doc_paged, options_anchor) = (doc.page_count.is_some(), options.start_anchor);
        let (fetched_tx, fetched_rx) = mpsc::channel();
        let kitty = (picker.protocol_type() == ProtocolType::Kitty && !picker.tmux_detected())
            .then(Kitty::default);
        App {
            image_mode: options.image_mode,
            highlights: options.highlights.clone(),
            bookmarks: options.bookmarks.clone(),
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
            selection: None,
            dragging: false,
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
            quit: false,
        }
    }

    fn pages(&self) -> usize {
        self.doc.pages()
    }

    fn last(&self) -> usize {
        self.pages().saturating_sub(1)
    }

    fn image_size(&self, href: &str) -> Option<(u32, u32)> {
        match Doc::image_ref(href) {
            ImageRef::Embedded(id) => self.doc.source.image_size(id),
            ImageRef::Remote(url) => match self.remote.get(url) {
                Some(Remote::Ready(image)) => Some(image.dimensions()),
                _ => None,
            },
        }
    }

    fn laid(&mut self, page: usize) -> Rc<Laid> {
        if let Some(laid) = self.laid.get(&page) {
            return laid.clone();
        }
        let markdown = self.doc.source.page(page);
        let font = self.picker.font_size();
        let sizes = |href: &str| self.image_size(href);
        let laid = Rc::new(render::layout(
            &markdown,
            self.width,
            (font.width, font.height),
            &sizes,
        ));
        self.laid.insert(page, laid.clone());
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
        self.doc.source.page(page).is_empty()
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
            None => format!("{}%", self.scrolled()),
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

    /// Where in a page-less document's text the top line of the screen sits.
    fn top_anchor(&mut self) -> usize {
        let laid = self.laid(0);
        let top = self
            .offset
            .saturating_sub(self.prefix(0))
            .min(laid.rows.len());
        laid.rows[top..]
            .iter()
            .flat_map(|row| &row.segs)
            .find_map(|seg| seg.offset)
            .unwrap_or(laid.text.len())
    }

    /// Scrolls a page-less document so the line holding `anchor` is at the top.
    fn scroll_to_anchor(&mut self, anchor: usize) {
        let laid = self.laid(0);
        let holds = |seg: &render::Seg| {
            seg.offset
                .is_some_and(|o| o + seg.text.chars().count() > anchor)
        };
        let row = laid
            .rows
            .iter()
            .position(|row| row.segs.iter().any(holds))
            .unwrap_or(laid.rows.len());
        self.offset = if row == 0 { 0 } else { row + self.prefix(0) };
    }

    /// Keeps a page-less document's top line in place while it lays out again.
    fn hold_anchor(&mut self) {
        if self.doc.page_count.is_none() && self.width > 0 && self.anchor.is_none() {
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
        if self.doc.page_count.is_none() {
            return self.report_scroll(false);
        }
        let page = self.reading() + 1;
        if self.reported != Some(page) {
            self.reported = Some(page);
            (self.options.on_page)(page);
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
        let width = MAX_TEXT_WIDTH.min(area.width.saturating_sub(4)).max(1);
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
        if let Some(anchor) = self.anchor.take() {
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
            };
            self.draw_row(f, page, row, self.column.y + y as u16, marks);
        }
        self.draw_figures(f, &screen);

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
            Dialog::Goto { digits, page_count } => {
                Some(self.draw_goto(f, area, &digits, page_count))
            }
            Dialog::Find { typed } => Some(self.draw_find(f, area, &typed)),
            Dialog::Bookmarks { selected } => Some(self.draw_bookmarks(f, area, selected)),
        };
        // Kitty images sit above the text, so any under a panel would hide it.
        if let Some(panel) = panel {
            self.placements.retain(|(_, rect)| !rect.intersects(panel));
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
                .or_insert_with(|| query.matches(&self.laid(page).text));
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
                let x = column.x + column.width.saturating_sub(label.chars().count() as u16) / 2;
                buf.set_string(x, y, label, Style::default().fg(DIM));
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
                    if highlights
                        .iter()
                        .any(|h| h.start <= offset && offset < h.end)
                    {
                        style = style.bg(MARK).fg(MARK_INK);
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
                let Some(pixels) = pixels else { continue };
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
                            kitty.upload(&slice, cols, rows).map(Drawn::Kitty)
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

    fn panel(f: &mut Frame, area: Rect, title: &str, lines: Vec<Line>, width: u16) -> Rect {
        let height = lines.len() as u16 + 4;
        let width = (width + 8).min(area.width);
        let rect = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height: height.min(area.height),
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
        }
        rows.extend([
            ("j/k  ↓/↑", "scroll"),
            ("d/u", "half screen down / up"),
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
        rows.push((
            "i",
            if self.image_mode == ImageMode::Color {
                "paint images"
            } else {
                "show original images"
            },
        ));
        rows.extend([
            ("drag", "select and copy text"),
            ("y", "highlight selection, or clear one"),
            ("?", "this help"),
            ("q", "quit"),
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
        Self::panel(f, area, "Keys", lines, width)
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
        Self::panel(f, area, "Go to page", lines, hint.chars().count() as u16)
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
        Self::panel(f, area, "Find", lines, width)
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
            return Self::panel(f, area, "Bookmarks", lines, 42);
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
        Self::panel(f, area, "Bookmarks", lines, width)
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
            let mut ranges = search
                .query
                .matches(&self.laid(page).text)
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

    pub fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        match self.dialog.clone() {
            Dialog::Goto { digits, page_count } => return self.goto_key(&key, digits, page_count),
            Dialog::Find { typed } => return self.find_key(&key, typed),
            Dialog::Bookmarks { selected } => return self.bookmarks_key(&key, selected),
            Dialog::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.dialog = Dialog::None;
                }
                return;
            }
            Dialog::None => {}
        }
        let paged = self.mode == Mode::Paged;
        match typed(&key) {
            Some('?') | Some('/') => self.dialog = Dialog::Help,
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
            Some('j') => self.scroll_by(1),
            Some('k') => self.scroll_by(-1),
            Some('d') => self.scroll_by(self.screen_step() / 2),
            Some('u') => self.scroll_by(-(self.screen_step() / 2)),
            Some(' ') => self.next_screen(paged),
            Some('b') => self.previous_screen(paged),
            Some('l') | Some('n') => self.go_to_page(self.current + 1, false),
            Some('h') | Some('p') => self.go_to_page(self.current.saturating_sub(1), false),
            Some('g') => self.go_to_page(0, false),
            Some('G') => self.go_to_page(self.last(), !paged),
            Some('i') => self.toggle_images(),
            _ => match key.code {
                // Esc lets go of a selection, then a search, then quits.
                KeyCode::Esc => {
                    if self.selection.take().is_none() && self.search.take().is_none() {
                        self.quit = true;
                    }
                }
                KeyCode::Down => self.scroll_by(1),
                KeyCode::Up => self.scroll_by(-1),
                KeyCode::PageDown => self.next_screen(paged),
                KeyCode::PageUp => self.previous_screen(paged),
                KeyCode::Right => self.go_to_page(self.current + 1, false),
                KeyCode::Left => self.go_to_page(self.current.saturating_sub(1), false),
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

    fn previous_screen(&mut self, paged: bool) {
        if paged && self.offset == 0 && self.current > 0 {
            self.go_to_page(self.current - 1, true);
        } else {
            self.scroll_by(-self.screen_step());
        }
    }

    fn toggle_images(&mut self) {
        self.image_mode = match self.image_mode {
            ImageMode::Color => ImageMode::Mono,
            ImageMode::Mono => ImageMode::Color,
        };
        (self.options.on_image_mode)(self.image_mode);
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

    pub fn mouse(&mut self, mouse: MouseEvent) {
        if self.dialog != Dialog::None {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = self.hit(mouse.column, mouse.row).map(|at| Selection {
                    anchor: at,
                    head: at,
                });
                self.dragging = self.selection.is_some();
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                if let (Some(selection), Some(head)) =
                    (self.selection, self.hit(mouse.column, mouse.row))
                {
                    self.selection = Some(Selection { head, ..selection });
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                let text = self.selected_text();
                if text.is_empty() {
                    self.selection = None;
                } else {
                    // Taking over the mouse turns off the terminal's own selection, so copy here.
                    self.copy = Some(text);
                }
            }
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_STEP),
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_STEP),
            _ => {}
        }
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

    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
        loop {
            self.receive_images();
            self.advance_search(SEARCH_SLICE);
            terminal.draw(|f| self.draw(f))?;
            if let Some(kitty) = &mut self.kitty {
                kitty.flush(&mut io::stdout(), &self.placements)?;
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
            // A search keeps reading pages between keys, and images
            // downloading are checked for often; otherwise sleep until something happens.
            let wait = if self.seeking() {
                Some(Duration::ZERO)
            } else {
                self.loading().then_some(Duration::from_millis(100))
            };
            if let Some(wait) = wait
                && !event::poll(wait)?
            {
                continue;
            }
            // Handle everything already waiting before drawing again.
            loop {
                match event::read()? {
                    Event::Key(key) => self.key(key),
                    Event::Mouse(mouse) => self.mouse(mouse),
                    // The redraw after a resize clears the screen, and kitty's placements with it.
                    Event::Resize(..) => {
                        if let Some(kitty) = &mut self.kitty {
                            kitty.reset();
                        }
                    }
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
fn query_color(code: u8) -> Option<Rgb> {
    let mut out = io::stdout();
    write!(out, "\x1b]{code};?\x1b\\").ok()?;
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
        let mut buf = [0u8; 64];
        // SAFETY: reads into a stack buffer of the length given.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        reply.extend_from_slice(&buf[..n as usize]);
        if reply.ends_with(b"\x07") || reply.ends_with(b"\x1b\\") {
            break;
        }
    }
    parse_color_reply(&String::from_utf8_lossy(&reply))
}

/// Reads "rgb:RRRR/GGGG/BBBB" (one to four hex digits a channel) from an OSC color reply.
fn parse_color_reply(reply: &str) -> Option<Rgb> {
    let rgb = reply.split("rgb:").nth(1)?;
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

/// Runs the reader in the terminal until the reader quits.
pub fn run(doc: Doc, options: ViewerOptions) -> io::Result<()> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    // The title names the terminal tab; on screen it would only take rows from the text.
    let setup = execute!(io::stdout(), EnableMouseCapture, SetTitle(&doc.title));
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut app = App::new(doc, options, picker);
    let result = setup.and_then(|_| app.event_loop(&mut terminal));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
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
    }

    impl Screen {
        fn new(doc: Doc, width: u16, height: u16, options: ViewerOptions) -> Self {
            let app = App::new(doc, options, Picker::halfblocks());
            let mut screen = Screen {
                app,
                terminal: Terminal::new(TestBackend::new(width, height)).expect("test terminal"),
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
            self.app.key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            self.draw()
        }

        fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) -> String {
            self.app.mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            });
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
        assert_eq!(*pages.borrow(), vec![2, 3]);
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
        let frame = s.draw();
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
        s.app.key(KeyEvent {
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
        assert_eq!(cell.bg, MARK);
        // Selecting inside it and pressing y again clears it.
        s.mouse(MouseEventKind::Down(MouseButton::Left), x + 1, y);
        s.mouse(MouseEventKind::Drag(MouseButton::Left), x + 3, y);
        s.mouse(MouseEventKind::Up(MouseButton::Left), x + 3, y);
        s.press(KeyCode::Char('y'));
        assert!(saved.borrow().is_empty());
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
        assert_eq!(parse_color_reply("garbage"), None);
    }
}
