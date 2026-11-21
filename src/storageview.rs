//! `leaf storage`: what takes the space on a drive, drawn in dither.
//!
//! The scan runs on other threads and the screen fills in as it goes: a
//! disk map where each category is a dithered band in the theme's colors,
//! meters and a donut that ease toward the numbers as they grow, and a list
//! of what was found, each with what leaf thinks it is and where most of it
//! sits. Anything can be opened and browsed, revealed, handed to the agent
//! pane to ask about, or deleted for good after a confirmation.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use dither_ratatui::dither_core::{Field, Matrix, Pie, PixelGrid, Rgba, Slice, Tone, ease};
use dither_ratatui::raster::{color, paint, pixel_size};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::art::{self, Ink};
use crate::junk::{Category, Status};
use crate::look::Look;
use crate::picker;
use crate::render::DIM;
use crate::scheme::Colors;
use crate::sweep::{self, Event as Found, Item, Progress, Scan};
use crate::volume::Volume;

/// Side margin, in columns.
const MARGIN: u16 = 3;
/// Width of the category column.
const SIDE: u16 = 36;
/// Rows the disk map takes.
const MAP_ROWS: u16 = 3;
/// How long the entrance takes.
const ENTRANCE: f32 = 0.9;
/// How quickly shown numbers catch up with the real ones, in seconds.
const CATCH_UP: f64 = 0.16;
/// A frame while something moves.
const FRAME: Duration = Duration::from_millis(33);
/// How long a message stays up.
const TOAST: Duration = Duration::from_secs(4);
/// Most items kept in a list; the rest are the smallest.
const LIST_CAP: usize = 2000;

// Formatting --------------------------------------------------------------

/// Bytes as Finder says them: decimal units.
pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e12 {
        format!("{:.1} TB", b / 1e12)
    } else if b >= 1e9 {
        format!("{:.1} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.0} KB", b / 1e3)
    } else {
        format!("{bytes} B")
    }
}

/// How long ago, shortly: today, 3d, 4mo, 2y.
fn age(days: Option<u64>) -> String {
    match days {
        None => String::new(),
        Some(0) => "today".to_owned(),
        Some(d) if d < 31 => format!("{d}d"),
        Some(d) if d < 365 => format!("{}mo", d / 30),
        Some(d) => format!("{}y", d / 365),
    }
}

/// 1204330 as 1,204,330.
fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `text` cut to `width` columns, with an ellipsis when cut.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
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

/// A path as shown: home as `~`.
fn shown_path(path: &Path) -> String {
    let text = path.display().to_string();
    // macOS's per-user temporary folder is a long hash; its name says more.
    if let Some(temp) = std::env::var_os("TMPDIR")
        .and_then(|t| std::fs::canonicalize(t).ok())
        .map(|t| t.display().to_string())
        && let Some(rest) = text.strip_prefix(temp.trim_end_matches('/'))
    {
        return format!("$TMPDIR{rest}");
    }
    picker::tilde(&text)
}

/// The name of a listed thing, with a slash on folders.
fn display_name(path: &Path, is_dir: bool) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if is_dir { format!("{name}/") } else { name }
}

// Colors ------------------------------------------------------------------

/// Everything drawn takes its colors from here, made from the theme.
struct Palette {
    light: bool,
    ink: Ink,
    tones: [Tone; Category::ALL.len()],
    good: Tone,
}

impl Palette {
    fn of(colors: Colors) -> Self {
        let hue = |pick: fn(&crate::theme::Theme) -> crate::mono::Rgb, fallback: f32| {
            colors
                .theme
                .and_then(|t| art::hue(pick(&t)))
                .unwrap_or(fallback)
        };
        let blue = hue(|t| t.blue, 250.0);
        let green = hue(|t| t.green, 145.0);
        let magenta = hue(|t| t.magenta, 320.0);
        let red = hue(|t| t.red, 25.0);
        let accent = hue(|t| t.accent, blue);
        let turn = |h: f32, by: f32| Tone::Hue((h + by).rem_euclid(360.0));
        let tones = [
            Tone::Hue(blue),     // Apps
            Tone::Hue(green),    // Developer
            Tone::Hue(magenta),  // Agents
            turn(magenta, 40.0), // Media
            turn(accent, -45.0), // Documents
            turn(red, 40.0),     // Caches
            Tone::Hue(red),      // Trash
            turn(green, 65.0),   // Other
            Tone::Grey,          // System
        ];
        Palette {
            light: colors.light,
            ink: Ink::of_colors(colors),
            tones,
            good: Tone::Hue(green),
        }
    }

    fn tone(&self, category: Category) -> Tone {
        self.tones[category.index()]
    }

    /// A pixel of `tone` at `strength` (0..1): brighter on a dark theme,
    /// deeper on a light one.
    fn pixel(&self, tone: Tone, strength: f32) -> Rgba {
        let s = strength.clamp(0.0, 1.0);
        if self.light {
            tone.shade(0.62 - 0.3 * s)
        } else {
            tone.shade(0.34 + 0.4 * s)
        }
    }

    /// Text in `tone`, readable on the page.
    fn text(&self, tone: Tone) -> Color {
        color(if self.light {
            tone.shade(0.28)
        } else {
            tone.shade(0.74)
        })
    }

    fn status(&self, status: Status) -> Style {
        match status {
            Status::Junk => Style::default().fg(self.text(self.good)),
            Status::Unused => Style::default().fg(self.text(self.tone(Category::Caches))),
            Status::InUse => Style::default().fg(self.text(self.tone(Category::Agents))),
            Status::Review => Style::default().fg(DIM),
        }
    }
}

// Dithered pieces ---------------------------------------------------------

/// A one-row dithered bar `value` (0..1) full, denser toward its end.
fn meter(buf: &mut Buffer, area: Rect, value: f64, tone: Tone, pal: &Palette, strong: bool) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let (w, h) = pixel_size(area);
    let filled = (value.clamp(0.0, 1.0) * w as f64).round() as usize;
    let filled = if value > 0.0 { filled.max(1) } else { 0 };
    let mut grid = PixelGrid::new(w, h);
    let boost = if strong { 1.0 } else { 0.7 };
    for x in 0..w {
        for y in 0..h {
            let (gx, gy) = (area.x as i32 + x as i32, area.y as i32 * 2 + y as i32);
            if x < filled {
                let t = (x + 1) as f32 / filled as f32;
                if x + 1 == filled || Matrix::Bayer4.on(gx, gy, (0.4 + 0.6 * t) * boost) {
                    grid.set(x as i32, y as i32, pal.pixel(tone, (0.3 + 0.7 * t) * boost));
                }
            } else if y == 1 && x % 2 == 0 {
                grid.set(x as i32, y as i32, pal.pixel(Tone::Grey, 0.0).fade(0.5));
            }
        }
    }
    paint(&grid, area, buf, pal.ink.paper());
}

/// A part of the disk map.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Band {
    Category(Category),
    /// Used, but not reached by the scan yet.
    Unknown,
    /// Used by everything outside the folder measured.
    Rest,
    Free,
}

/// The whole drive as one dithered strip: each category a band in its
/// tone, what's not yet scanned a moving field, free space a faint grain.
fn disk_map(buf: &mut Buffer, area: Rect, app: &App, pal: &Palette, time: f32) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let (w, h) = pixel_size(area);
    let volume = app.volume();
    let total = volume.total.max(1) as f64;
    let mut bands: Vec<(Band, f64)> = Category::ALL
        .iter()
        .filter(|c| **c != Category::System)
        .map(|&c| (Band::Category(c), app.shown[c.index()]))
        .collect();
    let scanned: f64 = bands.iter().map(|(_, b)| b).sum();
    if app.done && app.folder.is_some() {
        bands.push((Band::Rest, (volume.used() as f64 - scanned).max(0.0)));
    } else if app.done {
        bands.push((
            Band::Category(Category::System),
            app.shown[Category::System.index()],
        ));
    } else {
        bands.push((Band::Unknown, (volume.used() as f64 - scanned).max(0.0)));
    }
    bands.push((Band::Free, volume.free as f64));
    let mut spans = Vec::with_capacity(bands.len());
    let mut at = 0.0;
    for (band, bytes) in bands {
        let width = bytes / total * w as f64;
        spans.push((band, at, at + width));
        at += width;
    }
    let mut grid = PixelGrid::new(w, h);
    let mut span = 0;
    for x in 0..w {
        let middle = x as f64 + 0.5;
        while span + 1 < spans.len() && middle >= spans[span].2 {
            span += 1;
        }
        let (band, start, end) = spans[span];
        let local = ((middle - start) / (end - start).max(1e-9)) as f32;
        let gap = band != Band::Free && end - start > 3.0 && x == start.ceil() as usize;
        for y in 0..h {
            let (gx, gy) = (area.x as i32 + x as i32, area.y as i32 * 2 + y as i32);
            let v = y as f32 / h.max(1) as f32;
            let pixel = match band {
                _ if gap => None,
                Band::Category(c) => {
                    let picked = c == app.category;
                    let density = if picked { 0.92 } else { 0.55 } * (0.8 + 0.2 * (1.0 - local));
                    Matrix::Bayer8.on(gx, gy, density).then(|| {
                        let strength = if picked { 0.85 } else { 0.45 };
                        let lift = if Matrix::Bayer4.on(gx + 2, gy, 0.5) {
                            0.15
                        } else {
                            0.0
                        };
                        pal.pixel(pal.tone(c), strength + lift)
                    })
                }
                Band::Unknown => {
                    let u = x as f32 / w.max(1) as f32;
                    let wave = Field::Waves.sample(u, v, time * 0.8);
                    Matrix::Bayer8
                        .on(gx, gy, 0.12 + 0.4 * wave)
                        .then(|| pal.pixel(Tone::Grey, 0.2 + 0.5 * wave))
                }
                Band::Rest => Matrix::Bayer8
                    .on(gx, gy, 0.3)
                    .then(|| pal.pixel(Tone::Grey, 0.25)),
                Band::Free => Matrix::Bayer8
                    .on(gx, gy, 0.08)
                    .then(|| pal.pixel(Tone::Grey, 0.1)),
            };
            if let Some(pixel) = pixel {
                grid.set(x as i32, y as i32, pixel);
            }
        }
    }
    paint(&grid, area, buf, pal.ink.paper());
}

/// The categories as a dithered donut, the picked one lifted, its share
/// in the hole.
fn donut(buf: &mut Buffer, area: Rect, app: &App, pal: &Palette, progress: f32) {
    let slices: Vec<(Category, Slice)> = Category::ALL
        .iter()
        .filter(|c| app.shown[c.index()] > 0.0)
        .map(|&c| (c, Slice::new(c.name(), app.shown[c.index()], pal.tone(c))))
        .collect();
    if slices.is_empty() {
        return;
    }
    let cols = area.width.min(area.height * 2);
    let plot = Rect::new(area.x + (area.width - cols) / 2, area.y, cols, cols / 2);
    let only: Vec<Slice> = slices.iter().map(|(_, s)| s.clone()).collect();
    let mut pie = Pie::new(&only);
    pie.inner_radius = 0.62;
    pie.progress = progress;
    pie.hover = slices.iter().position(|(c, _)| *c == app.category);
    let (w, h) = pixel_size(plot);
    paint(&pie.render(w, h), plot, buf, pal.ink.paper());
    let used: f64 = slices.iter().map(|(_, s)| s.value).sum();
    let share = app.shown[app.category.index()] / used.max(1.0) * 100.0;
    let middle = plot.y + plot.height / 2;
    let center = |text: &str| plot.x + plot.width.saturating_sub(text.width() as u16) / 2;
    let big = format!("{share:.0}%");
    buf.set_string(
        center(&big),
        middle.saturating_sub(1),
        &big,
        Style::default().add_modifier(Modifier::BOLD),
    );
    let label = app.category.name();
    buf.set_string(center(label), middle, label, Style::default().fg(DIM));
}

// State -------------------------------------------------------------------

/// A folder being browsed.
struct Browse {
    /// Where browsing started; going back from here returns to the overview.
    root: PathBuf,
    path: PathBuf,
    entries: Vec<sweep::Entry>,
    receive: Receiver<Option<sweep::Entry>>,
    progress: Arc<Progress>,
    done: bool,
    cursor: usize,
    /// Whether the cursor was moved; until then it stays on the biggest.
    moved: bool,
    /// Where the cursor was in each folder above.
    trail: Vec<(PathBuf, usize)>,
}

impl Browse {
    fn open(root: PathBuf, path: PathBuf, trail: Vec<(PathBuf, usize)>) -> Self {
        let (progress, receive) = sweep::browse(&path);
        Browse {
            root,
            path,
            entries: Vec::new(),
            receive,
            progress,
            done: false,
            cursor: 0,
            moved: false,
            trail,
        }
    }

    fn drain(&mut self) {
        let picked = self.entries.get(self.cursor).map(|e| e.path.clone());
        let mut changed = false;
        loop {
            match self.receive.try_recv() {
                Ok(Some(entry)) => {
                    self.entries.push(entry);
                    changed = true;
                }
                Ok(None) | Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if changed {
            self.entries.sort_by_key(|e| std::cmp::Reverse(e.bytes));
            if let Some(picked) = picked.filter(|_| self.moved) {
                self.cursor = self
                    .entries
                    .iter()
                    .position(|e| e.path == picked)
                    .unwrap_or(0);
            }
        }
    }

    fn total(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }
}

/// Something to delete.
#[derive(Debug, Clone)]
struct Target {
    path: PathBuf,
    bytes: u64,
}

/// A deletion waiting for its yes.
#[derive(Debug, Clone)]
struct Plan {
    heading: String,
    targets: Vec<Target>,
    note: String,
}

impl Plan {
    fn bytes(&self) -> u64 {
        self.targets.iter().map(|t| t.bytes).sum()
    }
}

enum Deleted {
    One(Target),
    Failed(String),
    Done,
}

struct Deleting {
    plan: Plan,
    receive: Receiver<Deleted>,
    freed: u64,
    failed: Vec<String>,
}

enum Modal {
    Confirm(Plan),
    Deleting(Deleting),
    Drives { cursor: usize },
    Keys,
}

/// Where the mouse can click, from the last frame.
#[derive(Default)]
struct Hits {
    categories: Vec<(Rect, Category)>,
    rows: Vec<(Rect, usize)>,
}

struct App {
    volumes: Vec<Volume>,
    drive: usize,
    scan: Scan,
    items: Vec<Item>,
    done: bool,
    began: Instant,
    took: Option<Duration>,
    /// Bytes per category as drawn, easing toward the real numbers.
    shown: [f64; Category::ALL.len()],
    category: Category,
    junk_only: bool,
    /// Items of the picked category as listed, by index into `items`.
    visible: Vec<usize>,
    dirty: bool,
    cursor: usize,
    scroll: usize,
    browse: Option<Browse>,
    modal: Option<Modal>,
    toast: Option<(String, Instant)>,
    hits: Hits,
    now: i64,
    /// The one folder measured, when not a whole drive.
    folder: Option<PathBuf>,
}

impl App {
    fn new(volumes: Vec<Volume>, drive: usize, folder: Option<PathBuf>) -> Self {
        let scan = match &folder {
            Some(folder) => sweep::start_roots(vec![folder.clone()]),
            None => sweep::start(&volumes[drive], &volumes),
        };
        App {
            folder,
            volumes,
            drive,
            scan,
            items: Vec::new(),
            done: false,
            began: Instant::now(),
            took: None,
            shown: [0.0; Category::ALL.len()],
            category: Category::Caches,
            junk_only: false,
            visible: Vec::new(),
            dirty: true,
            cursor: 0,
            scroll: 0,
            browse: None,
            modal: None,
            toast: None,
            hits: Hits::default(),
            now: sweep::now(),
        }
    }

    fn volume(&self) -> &Volume {
        &self.volumes[self.drive]
    }

    /// Starts over, on `drive`.
    fn rescan(&mut self, drive: usize) {
        self.scan.cancel();
        self.volumes[drive] = self.volumes[drive].refreshed();
        let keep = self.category;
        let volumes = std::mem::take(&mut self.volumes);
        let folder = self.folder.take().filter(|_| drive == self.drive);
        *self = App::new(volumes, drive, folder);
        self.category = keep;
    }

    /// Takes in what the scan found since the last frame.
    fn drain(&mut self) {
        let budget = Instant::now();
        loop {
            match self.scan.events.try_recv() {
                Ok(Found::Found(item)) => {
                    self.items.push(item);
                    self.dirty = true;
                }
                Ok(Found::Done) | Err(TryRecvError::Disconnected) => {
                    if !self.done {
                        self.done = true;
                        self.took = Some(self.began.elapsed());
                        self.dirty = true;
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
            if budget.elapsed() > Duration::from_millis(8) {
                break;
            }
        }
        if let Some(browse) = &mut self.browse {
            browse.drain();
        }
    }

    /// The real bytes of a category now.
    fn bytes(&self, category: Category) -> f64 {
        let progress = &self.scan.progress;
        if category == Category::System && self.folder.is_some() {
            // Only the folder was measured; the rest of the drive isn't known.
            return progress.category(category) as f64;
        }
        if category == Category::System && self.done {
            let counted: u64 = Category::ALL
                .iter()
                .filter(|c| **c != Category::System)
                .map(|&c| progress.category(c))
                .sum();
            return self.volume().used().saturating_sub(counted) as f64;
        }
        progress.category(category) as f64
    }

    /// Eases the shown numbers toward the real ones. True while they move.
    fn ease(&mut self, dt: f64) -> bool {
        let mut moving = false;
        let k = 1.0 - (-dt / CATCH_UP).exp();
        for c in Category::ALL {
            let target = self.bytes(c);
            let now = &mut self.shown[c.index()];
            *now += (target - *now) * k;
            if (target - *now).abs() > target.max(1.0) * 0.002 + 1e5 {
                moving = true;
            } else {
                *now = target;
            }
        }
        moving
    }

    fn picked_path(&self) -> Option<PathBuf> {
        let &index = self.visible.get(self.cursor)?;
        Some(self.items[index].path.clone())
    }

    /// Lists the picked category again, keeping the cursor on its item.
    fn relist(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let picked = self.picked_path();
        let now = self.now;
        let mut visible: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.category == self.category)
            .filter(|(_, item)| {
                !self.junk_only || item.status(now).is_some_and(Status::reclaimable)
            })
            .map(|(i, _)| i)
            .collect();
        visible.sort_by(|&a, &b| self.items[b].own.cmp(&self.items[a].own));
        visible.truncate(LIST_CAP);
        self.visible = visible;
        if let Some(picked) = picked {
            self.cursor = self
                .visible
                .iter()
                .position(|&i| self.items[i].path == picked)
                .unwrap_or(self.cursor);
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    fn pick_category(&mut self, category: Category) {
        if self.category != category {
            self.category = category;
            self.cursor = 0;
            self.scroll = 0;
            self.dirty = true;
        }
    }

    fn step_category(&mut self, by: isize) {
        let all = Category::ALL;
        let at = self.category.index() as isize;
        let next = (at + by).rem_euclid(all.len() as isize) as usize;
        self.pick_category(all[next]);
    }

    /// Junk and unused bytes in the picked category.
    fn reclaimable(&self) -> (u64, u64) {
        let mut junk = 0;
        let mut unused = 0;
        for item in self.items.iter().filter(|i| i.category == self.category) {
            match item.status(self.now) {
                Some(Status::Junk) => junk += item.bytes,
                Some(Status::Unused) => unused += item.bytes,
                _ => {}
            }
        }
        (junk, unused)
    }

    fn say(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), Instant::now()));
    }

    /// The picked thing: in the folder browsed, or in the list.
    fn target(&self) -> Option<Target> {
        if let Some(browse) = &self.browse {
            let entry = browse.entries.get(browse.cursor)?;
            return Some(Target {
                path: entry.path.clone(),
                bytes: entry.bytes,
            });
        }
        let item = &self.items[*self.visible.get(self.cursor)?];
        Some(Target {
            path: item.path.clone(),
            bytes: item.bytes,
        })
    }

    /// Whether deleting `path` would take a whole drive or home with it.
    fn guarded(&self, path: &Path) -> bool {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        path.components().count() <= 2
            || home.as_deref() == Some(path)
            || self.volumes.iter().any(|v| v.mount == path)
            || self.scan.roots.iter().any(|r| r == path)
    }

    fn ask_delete(&mut self) {
        let Some(target) = self.target() else {
            return;
        };
        if self.guarded(&target.path) {
            self.say("leaf won't delete a drive, a home folder or a top-level folder.");
            return;
        }
        let why = self
            .items
            .iter()
            .find(|i| i.path == target.path)
            .and_then(|i| i.finding)
            .map(|f| format!("{}: {}.", f.label, f.why));
        let note = match why {
            Some(why) => format!("{why} It skips the Trash, so it can't be undone."),
            None => "It skips the Trash, so it can't be undone.".to_owned(),
        };
        self.modal = Some(Modal::Confirm(Plan {
            heading: format!(
                "Delete {} forever?",
                display_name(&target.path, target.path.is_dir())
            ),
            targets: vec![target],
            note,
        }));
    }

    fn ask_clear(&mut self) {
        let now = self.now;
        let mut targets: Vec<Target> = self
            .items
            .iter()
            .filter(|i| i.category == self.category && i.status(now) == Some(Status::Junk))
            .filter(|i| !self.guarded(&i.path))
            .map(|i| Target {
                path: i.path.clone(),
                bytes: i.bytes,
            })
            .collect();
        if targets.is_empty() {
            self.say(format!("Nothing in {} is junk.", self.category.name()));
            return;
        }
        targets.sort_by_key(|t| std::cmp::Reverse(t.bytes));
        self.modal = Some(Modal::Confirm(Plan {
            heading: format!(
                "Clear {} junk {} in {}?",
                targets.len(),
                if targets.len() == 1 { "item" } else { "items" },
                self.category.name()
            ),
            targets,
            note: "Only what rebuilds on its own. It skips the Trash, so it can't be undone."
                .to_owned(),
        }));
    }

    fn start_delete(&mut self, plan: Plan) {
        let (send, receive) = mpsc::channel();
        let targets = plan.targets.clone();
        std::thread::spawn(move || {
            for target in targets {
                let event = match sweep::delete(&target.path) {
                    Ok(()) => Deleted::One(target),
                    Err(e) => Deleted::Failed(e.to_string()),
                };
                let _ = send.send(event);
            }
            let _ = send.send(Deleted::Done);
        });
        self.modal = Some(Modal::Deleting(Deleting {
            plan,
            receive,
            freed: 0,
            failed: Vec::new(),
        }));
    }

    /// Takes in what the deletion finished since the last frame.
    fn drain_delete(&mut self) {
        let Some(Modal::Deleting(deleting)) = &mut self.modal else {
            return;
        };
        let mut gone = Vec::new();
        let mut finished = false;
        loop {
            match deleting.receive.try_recv() {
                Ok(Deleted::One(target)) => {
                    deleting.freed += target.bytes;
                    gone.push(target);
                }
                Ok(Deleted::Failed(why)) => deleting.failed.push(why),
                Ok(Deleted::Done) | Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        let summary = finished.then(|| {
            let freed = size(deleting.freed);
            match deleting.failed.first() {
                None => format!("Freed {freed}."),
                Some(first) => format!(
                    "Freed {freed}. {} couldn't be deleted: {first}. Press r to measure again.",
                    deleting.failed.len()
                ),
            }
        });
        for target in gone {
            self.forget(&target);
        }
        if let Some(summary) = summary {
            self.modal = None;
            self.volumes[self.drive] = self.volumes[self.drive].refreshed();
            self.say(summary);
        }
    }

    /// Drops what was deleted from the lists and the totals.
    fn forget(&mut self, target: &Target) {
        let progress = self.scan.progress.clone();
        let mut counted = false;
        self.items.retain(|item| {
            if item.path.starts_with(&target.path) {
                progress.remove(item.category, item.own);
                counted = true;
                false
            } else {
                true
            }
        });
        for item in &mut self.items {
            if target.path.starts_with(&item.path) {
                if !counted {
                    progress.remove(item.category, target.bytes);
                    counted = true;
                }
                item.bytes = item.bytes.saturating_sub(target.bytes);
                item.own = item.own.saturating_sub(target.bytes);
            }
        }
        if let Some(browse) = &mut self.browse {
            browse.entries.retain(|e| e.path != target.path);
            for entry in &mut browse.entries {
                if target.path.starts_with(&entry.path) {
                    entry.bytes = entry.bytes.saturating_sub(target.bytes);
                }
            }
            browse.cursor = browse.cursor.min(browse.entries.len().saturating_sub(1));
        }
        self.dirty = true;
    }

    fn open(&mut self) {
        if let Some(browse) = &mut self.browse {
            let Some(entry) = browse.entries.get(browse.cursor) else {
                return;
            };
            if !entry.is_dir {
                return;
            }
            let mut trail = std::mem::take(&mut browse.trail);
            trail.push((browse.path.clone(), browse.cursor));
            let root = browse.root.clone();
            let path = entry.path.clone();
            self.browse = Some(Browse::open(root, path, trail));
            return;
        }
        let Some(&index) = self.visible.get(self.cursor) else {
            return;
        };
        let item = &self.items[index];
        if item.is_dir {
            self.browse = Some(Browse::open(
                item.path.clone(),
                item.path.clone(),
                Vec::new(),
            ));
        }
    }

    /// Back up a folder, or out of browsing.
    fn back(&mut self) {
        let Some(browse) = &mut self.browse else {
            return;
        };
        match browse.trail.pop() {
            Some((path, cursor)) => {
                let trail = std::mem::take(&mut browse.trail);
                let root = browse.root.clone();
                let mut up = Browse::open(root, path, trail);
                up.cursor = cursor;
                up.moved = true;
                self.browse = Some(up);
            }
            None => self.browse = None,
        }
    }

    fn reveal(&mut self) {
        let Some(target) = self.target() else {
            return;
        };
        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open")
                .arg("-R")
                .arg(&target.path)
                .spawn()
        } else {
            let folder = target.path.parent().unwrap_or(&target.path).to_owned();
            std::process::Command::new("xdg-open").arg(folder).spawn()
        };
        if let Err(e) = result {
            self.say(format!("Couldn't show it in the file manager ({e})."));
        }
    }

    /// Hands the picked thing to the agent pane, asking what it is.
    fn ask_agent(&mut self) {
        let Some(target) = self.target() else {
            return;
        };
        let item = self.items.iter().find(|i| i.path == target.path);
        let mut text = format!(
            "leaf storage found this on my disk. What is it, is it safe to delete, and what's the cleanest way to free the space?\n\nPath: {}\nSize: {}",
            target.path.display(),
            size(target.bytes)
        );
        if let Some(item) = item {
            text.push_str(&format!(
                " in {} files, last changed {}\n",
                count(item.files),
                match item.age_days(self.now) {
                    Some(0) => "today".to_owned(),
                    Some(days) => format!("{days} days ago"),
                    None => "at an unknown time".to_owned(),
                }
            ));
            if let Some(finding) = item.finding {
                text.push_str(&format!(
                    "leaf thinks: {} ({}): {}\n",
                    item.status(self.now).map_or("", Status::word),
                    finding.label,
                    finding.why
                ));
            }
            if let Some(culprit) = &item.culprit {
                text.push_str(&format!(
                    "Mostly in: {} ({})\n",
                    culprit.path,
                    size(culprit.bytes)
                ));
            }
        } else {
            text.push('\n');
        }
        text.push_str("\nLook before answering, and don't delete anything without asking me.");
        match crate::agent::deliver(&text) {
            Ok(agent) => self.say(format!("Asked {}.", agent.describe())),
            Err(e) => self.say(format!("{e}")),
        }
    }

    fn grant_access(&mut self) {
        if !cfg!(target_os = "macos") {
            self.say("Folders owned by other accounts need sudo: sudo leaf storage.");
            return;
        }
        let terminal = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "your terminal".to_owned());
        let _ = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
            .spawn();
        self.say(format!(
            "Turn on {terminal} under Full Disk Access, restart it, then press r."
        ));
    }
}

// Drawing -----------------------------------------------------------------

fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, width: u16, style: Style) -> u16 {
    if y >= buf.area.bottom() || width == 0 {
        return x;
    }
    let (end, _) = buf.set_stringn(x, y, text, width as usize, style);
    end
}

/// `text` ending at column `right`.
fn put_right(buf: &mut Buffer, right: u16, y: u16, text: &str, style: Style) {
    let width = text.width() as u16;
    if right >= width {
        put(buf, right - width, y, text, width, style);
    }
}

/// Keys and what they do, keys plain and words dim, on one row.
fn keys(buf: &mut Buffer, x: u16, y: u16, width: u16, pairs: &[(&str, &str)]) {
    let mut at = x;
    let end = x + width;
    for (key, does) in pairs {
        let need = (key.width() + does.width() + 4) as u16;
        if at + need > end {
            break;
        }
        at = put(buf, at, y, key, end - at, Style::default());
        at = put(buf, at + 1, y, does, end - at, Style::default().fg(DIM));
        at += 3;
    }
}

fn spinner(time: f32) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(time * 12.0) as usize % FRAMES.len()]
}

fn draw(f: &mut Frame, app: &mut App, pal: &Palette, time: f32) {
    let area = f.area();
    app.hits = Hits::default();
    if area.width < 40 || area.height < 16 {
        f.buffer_mut().set_string(
            area.x,
            area.y,
            "Make the window bigger to see the storage screen.",
            Style::default().fg(DIM),
        );
        return;
    }
    let inner = Rect::new(
        area.x + MARGIN,
        area.y + 1,
        area.width - 2 * MARGIN,
        area.height - 1,
    );
    let entrance = ease::out_cubic(app.began.elapsed().as_secs_f32() / ENTRANCE);
    header(f.buffer_mut(), inner, app, time);
    let map = Rect::new(inner.x, inner.y + 3, inner.width, MAP_ROWS);
    disk_map(f.buffer_mut(), map, app, pal, time);

    let body_y = map.bottom() + 1;
    let footer_y = area.bottom() - 1;
    let body = Rect::new(
        inner.x,
        body_y,
        inner.width,
        footer_y.saturating_sub(body_y + 1),
    );
    if let Some(browse) = &app.browse {
        let tone = pal.tone(app.category);
        browse_view(
            f.buffer_mut(),
            body,
            browse,
            tone,
            app.now,
            pal,
            time,
            &mut app.hits.rows,
        );
    } else {
        let side = if body.width >= 96 {
            SIDE
        } else if body.width >= 72 {
            26
        } else {
            0
        };
        if side > 0 {
            categories(
                f.buffer_mut(),
                Rect {
                    width: side,
                    ..body
                },
                app,
                pal,
                entrance,
            );
        }
        let gap = if side > 0 { 3 } else { 0 };
        let detail_area = Rect::new(
            body.x + side + gap,
            body.y,
            body.width.saturating_sub(side + gap),
            body.height,
        );
        detail(f.buffer_mut(), detail_area, app, pal, entrance);
    }
    footer(
        f.buffer_mut(),
        Rect::new(inner.x, footer_y, inner.width, 1),
        app,
    );
    if let Some(modal) = &app.modal {
        draw_modal(f.buffer_mut(), area, modal, app, pal, time);
    }
}

fn header(buf: &mut Buffer, area: Rect, app: &App, time: f32) {
    let volume = app.volume();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let mut x = put(buf, area.x, area.y, "storage", area.width, bold);
    let place = match &app.folder {
        Some(folder) => format!("{}/  on {}", shown_path(folder), volume.name),
        None => volume.name.clone(),
    };
    let room = (area.width / 2).saturating_sub(x - area.x);
    x = put(buf, x + 2, area.y, &clip(&place, room as usize), room, dim);
    if app.volumes.len() > 1 {
        put(
            buf,
            x + 2,
            area.y,
            &format!("v {} drives", app.volumes.len()),
            12,
            dim,
        );
    }
    let unreadable = app.scan.progress.unreadable.load(Ordering::Relaxed);
    let status = if !app.done {
        let current = app
            .scan
            .progress
            .current
            .lock()
            .map(|p| shown_path(&p))
            .unwrap_or_default();
        format!(
            "{} {} files · {}",
            spinner(time),
            count(app.scan.progress.files.load(Ordering::Relaxed)),
            clip(&current, 40)
        )
    } else if unreadable > 0 {
        let fix = if cfg!(target_os = "macos") {
            "a grant Full Disk Access"
        } else {
            "run with sudo to read them"
        };
        format!("{} folders unreadable · {fix}", count(unreadable))
    } else {
        format!(
            "{} files in {:.1}s",
            count(app.scan.progress.files.load(Ordering::Relaxed)),
            app.took.map_or(0.0, |d| d.as_secs_f32())
        )
    };
    put_right(
        buf,
        area.right(),
        area.y,
        &clip(&status, (area.width / 2) as usize),
        dim,
    );

    let y = area.y + 1;
    let mut x = put(buf, area.x, y, &size(volume.used()), area.width, bold);
    x = put(
        buf,
        x + 1,
        y,
        &format!("used of {}", size(volume.total)),
        area.width,
        dim,
    );
    x = put(buf, x + 2, y, "·", 1, dim);
    x = put(buf, x + 2, y, &size(volume.free), area.width, bold);
    x = put(buf, x + 1, y, "free", 4, dim);
    x = put(buf, x + 2, y, "·", 1, dim);
    x = put(
        buf,
        x + 2,
        y,
        &format!("{:.0}%", volume.fullness() * 100.0),
        5,
        bold,
    );
    put(buf, x + 1, y, "full", 4, dim);
}

fn categories(buf: &mut Buffer, area: Rect, app: &mut App, pal: &Palette, entrance: f32) {
    let dim = Style::default().fg(DIM);
    put(buf, area.x + 2, area.y, "categories", area.width, dim);
    let largest = app.shown.iter().copied().fold(1.0, f64::max);
    let meter_width = if area.width >= SIDE { 10 } else { 0 };
    let mut y = area.y + 2;
    for c in Category::ALL {
        if y >= area.bottom() {
            break;
        }
        let picked = c == app.category;
        if picked {
            put(
                buf,
                area.x,
                y,
                "›",
                1,
                Style::default().fg(pal.text(pal.tone(c))),
            );
        }
        put(
            buf,
            area.x + 2,
            y,
            "■",
            1,
            Style::default().fg(pal.text(pal.tone(c))),
        );
        let name_style = if picked {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        put(buf, area.x + 4, y, c.name(), 12, name_style);
        let bytes = app.shown[c.index()];
        let text = if c == Category::System && !app.done {
            "…".to_owned()
        } else {
            size(bytes as u64)
        };
        let size_right = area.right() - if meter_width > 0 { meter_width + 2 } else { 0 };
        put_right(
            buf,
            size_right,
            y,
            &text,
            if picked { name_style } else { dim },
        );
        if meter_width > 0 {
            let value = bytes / largest * entrance as f64;
            meter(
                buf,
                Rect::new(area.right() - meter_width, y, meter_width, 1),
                value,
                pal.tone(c),
                pal,
                picked,
            );
        }
        app.hits
            .categories
            .push((Rect::new(area.x, y, area.width, 1), c));
        y += 1;
    }
    // The donut, when there's room for one worth seeing.
    let left = area.bottom().saturating_sub(y + 2);
    if left >= 9 {
        let height = left.min(14);
        let room = Rect::new(area.x + 2, y + 2, area.width - 2, height);
        donut(buf, room, app, pal, entrance);
    }
}

/// Rows the picked item takes when its details show.
fn detail_rows(item: &Item) -> usize {
    3 + usize::from(item.culprit.is_some()) + usize::from(item.own < item.bytes)
}

fn detail(buf: &mut Buffer, area: Rect, app: &mut App, pal: &Palette, entrance: f32) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let c = app.category;
    let tone = pal.tone(c);
    let mut y = area.y;
    put(
        buf,
        area.x,
        y,
        c.name(),
        area.width,
        Style::default()
            .fg(pal.text(tone))
            .add_modifier(Modifier::BOLD),
    );
    let bytes = app.shown[c.index()];
    let share = bytes / app.volume().total.max(1) as f64 * 100.0;
    let (junk, unused) = app.reclaimable();
    let verdict = if junk > 0 && junk as f64 >= bytes * 0.9 {
        Some(("safe to clear", pal.status(Status::Junk)))
    } else if c == Category::System {
        Some(("leave it to the system", dim))
    } else {
        None
    };
    let mut right = area.right();
    if let Some((word, style)) = verdict {
        put_right(buf, right, y, word, style);
        right = right.saturating_sub(word.width() as u16 + 3);
    }
    put_right(buf, right, y, &format!("{share:.0}% of disk"), dim);
    right = right.saturating_sub(format!("{share:.0}% of disk").width() as u16 + 2);
    put_right(buf, right, y, &size(bytes as u64), bold);
    y += 1;
    put(buf, area.x, y, c.about(), area.width, dim);
    y += 1;
    // What can go, and the keys that free it.
    let mut x = area.x;
    if junk > 0 {
        x = put(
            buf,
            x,
            y,
            &size(junk),
            area.width,
            pal.status(Status::Junk).add_modifier(Modifier::BOLD),
        );
        x = put(buf, x + 1, y, "junk", 4, dim);
    }
    if unused > 0 {
        if x > area.x {
            x = put(buf, x + 1, y, " ·", 2, dim);
            x += 1;
        }
        x = put(
            buf,
            x,
            y,
            &size(unused),
            area.width,
            pal.status(Status::Unused).add_modifier(Modifier::BOLD),
        );
        x = put(buf, x + 1, y, "unused", 6, dim);
    }
    let filter = if app.junk_only {
        "show everything"
    } else {
        "junk & unused only"
    };
    let hints = if junk > 0 {
        [
            format!("c clear junk   f {filter}"),
            "c clear · f filter".to_owned(),
        ]
    } else {
        [format!("f {filter}"), "f filter".to_owned()]
    };
    // The long hint, or the short one, or none: never over the totals.
    if let Some(hint) = hints
        .iter()
        .find(|h| x + 3 + h.width() as u16 <= area.right())
    {
        put_right(buf, area.right(), y, hint, dim);
    }
    y += 2;

    if app.visible.is_empty() {
        let text = if !app.done {
            "Looking…"
        } else if app.junk_only {
            "No junk or unused things here. f shows everything."
        } else if c == Category::System {
            "Nothing here leaf can list: it's the system itself, snapshots and folders it can't read."
        } else {
            "Nothing found here."
        };
        put(buf, area.x + 2, y, text, area.width.saturating_sub(2), dim);
        return;
    }
    put(
        buf,
        area.x + 2,
        y,
        &format!("{} items", count(app.visible.len() as u64)),
        area.width,
        dim,
    );
    put_right(buf, area.right(), y, "changed", dim);
    y += 1;
    let list = Rect::new(area.x, y, area.width, area.bottom().saturating_sub(y));
    items(buf, list, app, pal, entrance);
}

fn items(buf: &mut Buffer, area: Rect, app: &mut App, pal: &Palette, entrance: f32) {
    if area.height == 0 {
        return;
    }
    let rows = area.height as usize;
    // Keep the picked item and its details in view.
    let picked_rows = app
        .visible
        .get(app.cursor)
        .map_or(1, |&i| detail_rows(&app.items[i]));
    if app.cursor < app.scroll {
        app.scroll = app.cursor;
    }
    while app.scroll < app.cursor && (app.cursor - app.scroll) + picked_rows > rows {
        app.scroll += 1;
    }
    let wide = area.width >= 84;
    let largest = app.visible.first().map_or(1, |&i| app.items[i].own.max(1));
    let dim = Style::default().fg(DIM);
    let bar_width: u16 = if wide { 12 } else { 0 };
    // status · size · bar · share · age, from the right.
    let age_x = area.right() - 5;
    let share_x = age_x - if wide { 6 } else { 0 };
    let bar_x = share_x - if wide { bar_width + 1 } else { 0 };
    let size_x = bar_x - 9;
    let status_x = size_x - 9;
    let name_width = status_x.saturating_sub(area.x + 4);
    let total = app.shown[app.category.index()].max(1.0);
    let mut y = area.y;
    let mut row = app.scroll;
    while y < area.bottom() && row < app.visible.len() {
        let index = app.visible[row];
        let item = &app.items[index];
        let picked = row == app.cursor;
        let status = item.status(app.now);
        if picked {
            put(
                buf,
                area.x,
                y,
                "›",
                1,
                Style::default().fg(pal.text(pal.tone(item.category))),
            );
        }
        put(
            buf,
            area.x + 2,
            y,
            if item.is_dir { "▸" } else { "·" },
            1,
            dim,
        );
        let name = display_name(&item.path, item.is_dir);
        let name_style = if picked {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let x = put(
            buf,
            area.x + 4,
            y,
            &clip(&name, name_width as usize),
            name_width,
            name_style,
        );
        if let Some(parent) = item.path.parent() {
            let room = (area.x + 4 + name_width).saturating_sub(x + 3);
            put(
                buf,
                x + 2,
                y,
                &clip(&shown_path(parent), room as usize),
                room,
                dim,
            );
        }
        if let Some(status) = status {
            put(buf, status_x, y, status.word(), 8, pal.status(status));
        }
        put_right(
            buf,
            size_x + 8,
            y,
            &size(item.own),
            if picked { name_style } else { Style::default() },
        );
        if wide {
            let value = item.own as f64 / largest as f64 * entrance as f64;
            meter(
                buf,
                Rect::new(bar_x, y, bar_width, 1),
                value,
                pal.tone(item.category),
                pal,
                picked,
            );
            let share = (item.own as f64 / total * 100.0).round();
            put_right(buf, share_x + 4, y, &format!("{share:.0}%"), dim);
        }
        let days = item.age_days(app.now);
        let old = days.is_some_and(|d| d >= 180);
        let age_style = if old { pal.status(Status::Unused) } else { dim };
        put_right(buf, area.right(), y, &age(days), age_style);
        app.hits
            .rows
            .push((Rect::new(area.x, y, area.width, 1), row));
        y += 1;
        if picked {
            y = item_details(
                buf,
                Rect::new(area.x + 4, y, area.width - 4, area.bottom() - y),
                item,
                status,
                pal,
                app.now,
            );
        }
        row += 1;
    }
}

/// What's known about the picked item, under its row. Returns the next free row.
fn item_details(
    buf: &mut Buffer,
    area: Rect,
    item: &Item,
    status: Option<Status>,
    pal: &Palette,
    now: i64,
) -> u16 {
    let dim = Style::default().fg(DIM);
    let mut y = area.y;
    if y >= area.bottom() {
        return y;
    }
    let mut x = area.x;
    if let Some(status) = status {
        x = put(buf, x, y, status.word(), 8, pal.status(status));
        x = put(buf, x + 1, y, "·", 1, dim) + 1;
    }
    let what = match item.finding {
        Some(f) => format!("{} · {}", f.label, f.why),
        None => format!(
            "{} files · changed {}",
            count(item.files),
            match item.age_days(now) {
                Some(0) => "today".to_owned(),
                Some(d) => format!("{} ago", age(Some(d))),
                None => "at an unknown time".to_owned(),
            }
        ),
    };
    put(buf, x, y, &what, area.right().saturating_sub(x), dim);
    y += 1;
    if item.own < item.bytes && y < area.bottom() {
        let text = format!(
            "{} in all, with {} listed on its own inside",
            size(item.bytes),
            size(item.bytes - item.own)
        );
        put(buf, area.x, y, &text, area.width, dim);
        y += 1;
    }
    if let Some(culprit) = &item.culprit
        && y < area.bottom()
    {
        let share = culprit.bytes as f64 / item.bytes.max(1) as f64 * 100.0;
        let mut x = put(buf, area.x, y, "└ mostly in ", area.width, dim);
        let room = area.right().saturating_sub(x + 20) as usize;
        x = put(
            buf,
            x,
            y,
            &clip(&culprit.path, room),
            room as u16,
            Style::default(),
        );
        x = put(buf, x + 2, y, &size(culprit.bytes), 9, Style::default());
        x = put(buf, x + 1, y, &format!("· {share:.0}%"), 6, dim);
        if culprit.cache {
            put(buf, x + 2, y, "cache", 5, pal.status(Status::Junk));
        }
        y += 1;
    }
    if y < area.bottom() {
        let mut pairs = Vec::new();
        if item.is_dir {
            pairs.push(("→", "open"));
        }
        pairs.extend([("o", "reveal"), ("@", "ask agent"), ("d", "delete")]);
        keys(buf, area.x, y, area.width, &pairs);
        y += 1;
    }
    y + 1
}

#[allow(clippy::too_many_arguments)]
fn browse_view(
    buf: &mut Buffer,
    area: Rect,
    browse: &Browse,
    tone: Tone,
    now: i64,
    pal: &Palette,
    time: f32,
    hits: &mut Vec<(Rect, usize)>,
) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    let mut y = area.y;
    let here = shown_path(&browse.path);
    let x = put(
        buf,
        area.x,
        y,
        &clip(&format!("{here}/"), area.width as usize - 24),
        area.width,
        bold,
    );
    let total = browse.total();
    let status = if browse.done {
        size(total)
    } else {
        format!(
            "{} {}",
            spinner(time),
            size(browse.progress.bytes.load(Ordering::Relaxed).max(total))
        )
    };
    put_right(
        buf,
        area.right(),
        y,
        &status,
        if browse.done { bold } else { dim },
    );
    let _ = x;
    y += 2;
    if browse.entries.is_empty() {
        put(
            buf,
            area.x + 2,
            y,
            if browse.done {
                "Empty."
            } else {
                "Measuring…"
            },
            area.width,
            dim,
        );
        return;
    }
    let rows = area.bottom().saturating_sub(y) as usize;
    let first = browse.cursor.saturating_sub(rows.saturating_sub(1));
    let largest = browse.entries.first().map_or(1, |e| e.bytes.max(1));
    let wide = area.width >= 70;
    let bar_width = if wide { 20 } else { 0 };
    let age_x = area.right() - 5;
    let bar_x = age_x - if wide { bar_width + 2 } else { 0 };
    let size_x = bar_x - 9;
    let label_x = size_x - 22;
    let name_width = label_x.saturating_sub(area.x + 4);
    for (row, entry) in browse.entries.iter().enumerate().skip(first).take(rows) {
        let picked = row == browse.cursor;
        if picked {
            put(buf, area.x, y, "›", 1, Style::default().fg(pal.text(tone)));
        }
        put(
            buf,
            area.x + 2,
            y,
            if entry.is_dir { "▸" } else { "·" },
            1,
            dim,
        );
        let style = if picked { bold } else { Style::default() };
        put(
            buf,
            area.x + 4,
            y,
            &clip(
                &display_name(&entry.path, entry.is_dir),
                name_width as usize,
            ),
            name_width,
            style,
        );
        if let Some(finding) = entry.finding {
            let status = crate::junk::status(Some(&finding), None);
            let style = status.map_or(dim, |s| pal.status(s));
            put(buf, label_x, y, &clip(finding.label, 20), 20, style);
        }
        put_right(buf, size_x + 8, y, &size(entry.bytes), style);
        if wide {
            meter(
                buf,
                Rect::new(bar_x, y, bar_width, 1),
                entry.bytes as f64 / largest as f64,
                tone,
                pal,
                picked,
            );
        }
        let days =
            (entry.newest > 0).then(|| (now.saturating_sub(entry.newest).max(0) / 86_400) as u64);
        put_right(buf, area.right(), y, &age(days), dim);
        hits.push((Rect::new(area.x, y, area.width, 1), row));
        y += 1;
    }
}

fn footer(buf: &mut Buffer, area: Rect, app: &App) {
    if let Some((text, at)) = &app.toast
        && at.elapsed() < TOAST
    {
        put(
            buf,
            area.x,
            area.y,
            &clip(text, area.width as usize),
            area.width,
            Style::default().add_modifier(Modifier::BOLD),
        );
        return;
    }
    let pairs: &[(&str, &str)] = if app.browse.is_some() {
        &[
            ("↑↓", "move"),
            ("→", "open"),
            ("←", "back"),
            ("o", "reveal"),
            ("@", "ask agent"),
            ("d", "delete"),
            ("q", "quit"),
        ]
    } else {
        &[
            ("↑↓", "move"),
            ("⇥", "category"),
            ("→", "open"),
            ("d", "delete"),
            ("c", "clear junk"),
            ("@", "ask agent"),
            ("v", "drives"),
            ("r", "rescan"),
            ("?", "keys"),
            ("q", "quit"),
        ]
    };
    keys(buf, area.x, area.y, area.width, pairs);
}

/// A panel floating over the screen, tinted, with a dithered shadow.
fn panel(buf: &mut Buffer, screen: Rect, width: u16, height: u16, pal: &Palette) -> Rect {
    let width = width.min(screen.width.saturating_sub(4));
    let height = height.min(screen.height.saturating_sub(2));
    let rect = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    for y in rect.top()..rect.bottom() {
        for x in rect.left()..rect.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
    art::surface(buf, rect, pal.ink);
    art::shadow(buf, rect, pal.ink);
    Rect::new(
        rect.x + 3,
        rect.y + 1,
        rect.width.saturating_sub(6),
        rect.height.saturating_sub(2),
    )
}

fn draw_modal(buf: &mut Buffer, screen: Rect, modal: &Modal, app: &App, pal: &Palette, time: f32) {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(DIM);
    match modal {
        Modal::Confirm(plan) => {
            let shown = plan.targets.len().min(5);
            let more = plan.targets.len() > shown;
            let height = 8 + shown as u16 + u16::from(more);
            let inner = panel(buf, screen, 72, height, pal);
            let mut y = inner.y;
            put(buf, inner.x, y, &plan.heading, inner.width, bold);
            y += 2;
            for target in plan.targets.iter().take(shown) {
                let path = shown_path(&target.path);
                put(
                    buf,
                    inner.x,
                    y,
                    &clip(&path, inner.width as usize - 10),
                    inner.width - 10,
                    Style::default(),
                );
                put_right(buf, inner.right(), y, &size(target.bytes), bold);
                y += 1;
            }
            if more {
                put(
                    buf,
                    inner.x,
                    y,
                    &format!("and {} more", plan.targets.len() - shown),
                    inner.width,
                    dim,
                );
                y += 1;
            }
            y += 1;
            put(
                buf,
                inner.x,
                y,
                &clip(&plan.note, inner.width as usize),
                inner.width,
                dim,
            );
            y += 2;
            let x = put(buf, inner.x, y, "y", 1, bold);
            let x = put(
                buf,
                x + 1,
                y,
                &format!("delete {}", size(plan.bytes())),
                20,
                dim,
            );
            let x = put(buf, x + 3, y, "n", 1, bold);
            put(buf, x + 1, y, "keep", 4, dim);
        }
        Modal::Deleting(deleting) => {
            let inner = panel(buf, screen, 60, 9, pal);
            put(buf, inner.x, inner.y, "Deleting…", inner.width, bold);
            let field = Rect::new(inner.x, inner.y + 2, inner.width, 3);
            art::field(buf, field, pal.ink, Field::Ripple, time);
            let total = deleting.plan.bytes().max(1);
            let line = format!("{} of {}", size(deleting.freed), size(total));
            put(buf, inner.x, inner.y + 6, &line, inner.width, dim);
            meter(
                buf,
                Rect::new(inner.right().saturating_sub(16), inner.y + 6, 16, 1),
                deleting.freed as f64 / total as f64,
                pal.good,
                pal,
                true,
            );
        }
        Modal::Drives { cursor } => {
            let inner = panel(buf, screen, 70, app.volumes.len() as u16 * 2 + 5, pal);
            put(buf, inner.x, inner.y, "Drives", inner.width, bold);
            let mut y = inner.y + 2;
            for (i, volume) in app.volumes.iter().enumerate() {
                let picked = i == *cursor;
                if picked {
                    put(buf, inner.x, y, "›", 1, bold);
                }
                let x = put(
                    buf,
                    inner.x + 2,
                    y,
                    &volume.name,
                    24,
                    if picked { bold } else { Style::default() },
                );
                put(
                    buf,
                    x + 2,
                    y,
                    &clip(&volume.mount.display().to_string(), 20),
                    20,
                    dim,
                );
                let text = format!("{} free of {}", size(volume.free), size(volume.total));
                put_right(buf, inner.right(), y, &text, dim);
                meter(
                    buf,
                    Rect::new(inner.x + 2, y + 1, inner.width.saturating_sub(2), 1),
                    volume.fullness(),
                    if volume.internal {
                        pal.tone(Category::Apps)
                    } else {
                        pal.tone(Category::Documents)
                    },
                    pal,
                    picked,
                );
                y += 2;
            }
        }
        Modal::Keys => {
            let lines: &[(&str, &str)] = &[
                ("↑↓ j k", "move through the list"),
                ("⇥ ⇧⇥ [ ] 1-9", "pick a category"),
                ("→ enter l", "open a folder and browse it"),
                ("← ⌫ h", "back up"),
                ("o", "reveal in the file manager"),
                ("@", "ask the agent pane what it is"),
                ("d", "delete forever, after a yes"),
                ("c", "clear every junk item in the category"),
                ("f", "junk and unused only"),
                ("v", "pick another drive"),
                ("r", "measure again"),
                ("a", "grant Full Disk Access (macOS)"),
                ("q esc", "quit"),
            ];
            let inner = panel(buf, screen, 60, lines.len() as u16 + 4, pal);
            put(buf, inner.x, inner.y, "Keys", inner.width, bold);
            for (i, (key, does)) in lines.iter().enumerate() {
                let y = inner.y + 2 + i as u16;
                put(buf, inner.x, y, key, 16, Style::default());
                put(
                    buf,
                    inner.x + 16,
                    y,
                    does,
                    inner.width.saturating_sub(16),
                    dim,
                );
            }
        }
    }
}

// Input -------------------------------------------------------------------

/// What a key or click asks for.
enum Step {
    Go,
    Quit,
}

fn modal_key(app: &mut App, code: KeyCode) {
    let Some(modal) = app.modal.take() else {
        return;
    };
    match modal {
        Modal::Confirm(plan) => match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.start_delete(plan),
            KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => {}
            _ => app.modal = Some(Modal::Confirm(plan)),
        },
        // Deleting can't be stopped halfway.
        deleting @ Modal::Deleting(_) => app.modal = Some(deleting),
        Modal::Drives { cursor } => match code {
            KeyCode::Down | KeyCode::Char('j') => {
                app.modal = Some(Modal::Drives {
                    cursor: (cursor + 1).min(app.volumes.len() - 1),
                })
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.modal = Some(Modal::Drives {
                    cursor: cursor.saturating_sub(1),
                })
            }
            KeyCode::Enter => {
                if cursor != app.drive {
                    app.rescan(cursor);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('v') => {}
            _ => app.modal = Some(Modal::Drives { cursor }),
        },
        Modal::Keys => {}
    }
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Step {
    if app.modal.is_some() {
        modal_key(app, code);
        return Step::Go;
    }
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Char('c') if ctrl => return Step::Quit,
        KeyCode::Char('q') => return Step::Quit,
        KeyCode::Esc => {
            if app.browse.is_some() {
                app.browse = None;
            } else {
                return Step::Quit;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => move_by(app, 1),
        KeyCode::Up | KeyCode::Char('k') => move_by(app, -1),
        KeyCode::PageDown => move_by(app, 10),
        KeyCode::PageUp => move_by(app, -10),
        KeyCode::Char('g') | KeyCode::Home => move_by(app, isize::MIN / 2),
        KeyCode::Char('G') | KeyCode::End => move_by(app, isize::MAX / 2),
        KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => app.open(),
        KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => app.back(),
        KeyCode::Tab | KeyCode::Char(']') if app.browse.is_none() => app.step_category(1),
        KeyCode::BackTab | KeyCode::Char('[') if app.browse.is_none() => app.step_category(-1),
        KeyCode::Char(n @ '1'..='9') if app.browse.is_none() => {
            let index = n as usize - '1' as usize;
            if let Some(&c) = Category::ALL.get(index) {
                app.pick_category(c);
            }
        }
        KeyCode::Char('d') | KeyCode::Delete => app.ask_delete(),
        KeyCode::Char('c') if app.browse.is_none() => app.ask_clear(),
        KeyCode::Char('f') => {
            app.junk_only = !app.junk_only;
            app.dirty = true;
        }
        KeyCode::Char('o') => app.reveal(),
        KeyCode::Char('@') => app.ask_agent(),
        KeyCode::Char('v') => {
            app.modal = Some(Modal::Drives { cursor: app.drive });
        }
        KeyCode::Char('r') => app.rescan(app.drive),
        KeyCode::Char('a') => app.grant_access(),
        KeyCode::Char('?') => app.modal = Some(Modal::Keys),
        _ => {}
    }
    Step::Go
}

fn move_by(app: &mut App, by: isize) {
    let (cursor, len) = match &mut app.browse {
        Some(browse) => {
            browse.moved = true;
            (&mut browse.cursor, browse.entries.len())
        }
        None => (&mut app.cursor, app.visible.len()),
    };
    if len == 0 {
        return;
    }
    let next = (*cursor as isize)
        .saturating_add(by)
        .clamp(0, len as isize - 1);
    *cursor = next as usize;
}

fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
    if app.modal.is_some() {
        return;
    }
    let hit = |rect: &Rect| rect.contains((column, row).into());
    match kind {
        MouseEventKind::ScrollDown => move_by(app, 3),
        MouseEventKind::ScrollUp => move_by(app, -3),
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(&(_, c)) = app.hits.categories.iter().find(|(r, _)| hit(r)) {
                app.pick_category(c);
            } else if let Some(&(_, at)) = app.hits.rows.iter().find(|(r, _)| hit(r)) {
                let current = match &app.browse {
                    Some(browse) => browse.cursor,
                    None => app.cursor,
                };
                if current == at {
                    app.open();
                } else {
                    move_by(app, at as isize - current as isize);
                }
            }
        }
        _ => {}
    }
}

/// Shows the storage screen for `volumes[drive]` until it's closed.
pub fn run(volumes: Vec<Volume>, drive: usize, folder: Option<PathBuf>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(
        io::stdout(),
        EnableMouseCapture,
        crossterm::terminal::SetTitle("Storage")
    );
    let result = (|| {
        let mut look = Look::detect();
        let mut app = App::new(volumes, drive, folder);
        let mut last = Instant::now();
        loop {
            let dt = last.elapsed().as_secs_f64();
            last = Instant::now();
            app.now = sweep::now();
            app.drain();
            app.drain_delete();
            app.relist();
            let moving = app.ease(dt);
            let pal = Palette::of(look.colors());
            let time = art::clock();
            terminal.draw(|f| draw(f, &mut app, &pal, time))?;
            let entering = app.began.elapsed().as_secs_f32() < ENTRANCE;
            let browsing = app.browse.as_ref().is_some_and(|b| !b.done);
            let toast = app
                .toast
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < TOAST);
            let animating = !app.done
                || moving
                || entering
                || browsing
                || matches!(app.modal, Some(Modal::Deleting(_)));
            let wait = if animating {
                FRAME
            } else if toast {
                Duration::from_millis(250)
            } else {
                look.due().min(Duration::from_secs(1))
            };
            if event::poll(wait)? {
                // Everything waiting, then one frame for all of it.
                loop {
                    match event::read()? {
                        Event::Key(k) if k.kind != KeyEventKind::Release => {
                            if let Step::Quit = key(&mut app, k.code, k.modifiers) {
                                app.scan.cancel();
                                return Ok(());
                            }
                            app.relist();
                        }
                        Event::Mouse(m) => mouse(&mut app, m.kind, m.column, m.row),
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            look.refresh(!animating);
        }
    })();
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

// For agents --------------------------------------------------------------

#[derive(serde::Serialize)]
pub struct Report {
    volume: Volume,
    used: u64,
    seconds: f64,
    files: u64,
    unreadable: u64,
    categories: Vec<CategoryReport>,
    items: Vec<ItemReport>,
}

#[derive(serde::Serialize)]
struct CategoryReport {
    category: Category,
    name: &'static str,
    bytes: u64,
}

#[derive(serde::Serialize)]
struct ItemReport {
    path: PathBuf,
    category: Category,
    bytes: u64,
    files: u64,
    status: Option<Status>,
    label: Option<&'static str>,
    why: Option<&'static str>,
    changed_days_ago: Option<u64>,
    mostly_in: Option<sweep::Culprit>,
}

/// Measures `volume` to the end, for `leaf storage --json`: the categories,
/// and the biggest things found, findings first.
pub fn report(volume: &Volume, all: &[Volume], folder: Option<PathBuf>) -> Report {
    let began = Instant::now();
    let measured_folder = folder.is_some();
    let scan = match folder {
        Some(folder) => sweep::start_roots(vec![folder]),
        None => sweep::start(volume, all),
    };
    let mut items = sweep::scan_to_end(&scan);
    let progress = &scan.progress;
    let mut categories: Vec<CategoryReport> = Category::ALL
        .iter()
        .filter(|c| **c != Category::System)
        .map(|&c| CategoryReport {
            category: c,
            name: c.name(),
            bytes: progress.category(c),
        })
        .collect();
    let counted: u64 = categories.iter().map(|c| c.bytes).sum();
    categories.push(CategoryReport {
        category: Category::System,
        name: Category::System.name(),
        bytes: if measured_folder {
            progress.category(Category::System)
        } else {
            volume.used().saturating_sub(counted)
        },
    });
    let now = sweep::now();
    items.sort_by(|a, b| {
        b.finding
            .is_some()
            .cmp(&a.finding.is_some())
            .then(b.bytes.cmp(&a.bytes))
    });
    items.retain(|i| i.bytes >= 10_000_000);
    items.truncate(300);
    items.sort_by_key(|i| std::cmp::Reverse(i.bytes));
    Report {
        volume: volume.clone(),
        used: volume.used(),
        seconds: began.elapsed().as_secs_f64(),
        files: progress.files.load(Ordering::Relaxed),
        unreadable: progress.unreadable.load(Ordering::Relaxed),
        categories,
        items: items
            .into_iter()
            .map(|item| ItemReport {
                status: item.status(now),
                changed_days_ago: item.age_days(now),
                label: item.finding.map(|f| f.label),
                why: item.finding.map(|f| f.why),
                path: item.path,
                category: item.category,
                bytes: item.bytes,
                files: item.files,
                mostly_in: item.culprit,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_like_finder() {
        assert_eq!(size(424_400_000_000), "424.4 GB");
        assert_eq!(size(981_000_000), "981 MB");
        assert_eq!(size(2_100_000_000_000), "2.1 TB");
        assert_eq!(size(512), "512 B");
        assert_eq!(count(1_204_330), "1,204,330");
        assert_eq!(count(12), "12");
        assert_eq!(age(Some(0)), "today");
        assert_eq!(age(Some(14)), "14d");
        assert_eq!(age(Some(200)), "6mo");
        assert_eq!(age(Some(800)), "2y");
        assert_eq!(age(None), "");
    }

    #[test]
    fn the_disk_map_paints_each_category_in_its_own_band() {
        let pal = Palette::of(Colors::DARK);
        let volume = Volume {
            name: "Disk".to_owned(),
            mount: PathBuf::from("/"),
            total: 1000,
            free: 500,
            internal: true,
        };
        let mut app = App::new(vec![volume], 0, None);
        app.scan.cancel();
        app.done = true;
        app.shown = [0.0; Category::ALL.len()];
        app.shown[Category::Caches.index()] = 500.0;
        let area = Rect::new(0, 0, 40, 3);
        let mut buf = Buffer::empty(area);
        disk_map(&mut buf, area, &app, &pal, 0.0);
        let lit = |x0: u16, x1: u16| {
            (x0..x1)
                .flat_map(|x| (0..3).map(move |y| (x, y)))
                .filter(|&(x, y)| buf[(x, y)].symbol() != " ")
                .count()
        };
        assert!(
            lit(1, 19) > lit(21, 40) * 3,
            "the full half is dense, free space faint"
        );
    }

    #[test]
    fn deleting_drops_what_was_inside_from_the_lists_and_totals() {
        let volume = Volume {
            name: "Disk".to_owned(),
            mount: PathBuf::from("/"),
            total: 1000,
            free: 500,
            internal: true,
        };
        let mut app = App::new(vec![volume], 0, None);
        app.scan.cancel();
        let item = |path: &str, bytes| Item {
            path: PathBuf::from(path),
            is_dir: true,
            category: Category::Caches,
            finding: None,
            bytes,
            own: bytes,
            files: 1,
            newest: 0,
            culprit: None,
        };
        app.items = vec![item("/a/cache", 300), item("/a/other", 50)];
        app.scan.progress.remove(Category::Caches, 0);
        app.forget(&Target {
            path: PathBuf::from("/a/cache"),
            bytes: 300,
        });
        assert_eq!(app.items.len(), 1);
        assert_eq!(app.items[0].path, PathBuf::from("/a/other"));
        assert!(app.guarded(Path::new("/Users")));
        assert!(!app.guarded(Path::new("/Users/a/Library/Caches/x")));
    }
}
