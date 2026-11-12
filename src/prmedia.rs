//! Pictures in a pull request's description, drawn the way the reader draws
//! figures: straight onto the screen with the Kitty graphics protocol. Images
//! show as they are; a video shows its first frame (taken with ffmpeg when
//! it's installed) under a play button, and a click opens it. A terminal
//! without Kitty graphics keeps a one-line placeholder for each.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use image::{Rgba, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use regex::{Captures, Regex};
use resvg::{tiny_skia, usvg};

use crate::kitty::{self, Kitty};
use crate::render::Figure;
use crate::video::{self, PlayFailed, Playback};

const USER_AGENT: &str = "Mozilla/5.0 (leaf reader)";
/// An image larger than this is left as a placeholder rather than held in memory.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const UNPLAYABLE: &str = "The video couldn't be played: it didn't load, or ffmpeg can't read it. Click it to try again, or press o to open the pull request in the browser.";
/// Frames kept of one GIF; the rest are left out of the loop.
const MAX_GIF_FRAMES: usize = 600;
/// Bytes of decoded frames kept of one GIF.
const MAX_GIF_BYTES: usize = 192 * 1024 * 1024;
/// Bytes of decoded GIF frames one gallery holds at once, across all its GIFs.
const GALLERY_GIF_BYTES: usize = 512 * 1024 * 1024;
/// How often the screen is drawn while a video starts or plays.
const VIDEO_REDRAW: Duration = Duration::from_millis(30);
/// How long the signed upload links in a rendered body are trusted; GitHub's
/// last about five minutes.
pub const SIGNED_LINKS_LAST: Duration = Duration::from_secs(4 * 60);
/// A GIF frame asking for less than this shows for `GIF_DEFAULT_DELAY`, as browsers do.
const GIF_MIN_DELAY: Duration = Duration::from_millis(20);
const GIF_DEFAULT_DELAY: Duration = Duration::from_millis(100);
/// How long the loading ring takes per step, of `RING_STEPS` a turn.
const RING_STEP: Duration = Duration::from_millis(80);
const RING_STEPS: u64 = 12;
/// An SVG's longer side is drawn at most this many pixels.
const MAX_SVG_SIDE: f32 = 4096.0;
/// The card a video gets when no frame of it can be taken.
const CARD: (u32, u32) = (640, 360);

static IMG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<img\b[^>]*>").expect("pattern is valid"));
static VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<video\b[^>]*>(?:.*?</video>)?").expect("pattern is valid"));
static ATTR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b(src|alt|width|height)\s*=\s*["']([^"']*)["']"#).expect("pattern is valid")
});
/// A link on a line of its own to an upload or a video file: GitHub shows these as players.
/// An upload's id: the UUID in both its github.com link and its signed link.
static UPLOAD_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
        .expect("pattern is valid")
});
/// A signed link to an upload in a private repository, as the rendered body has it.
static SIGNED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"https://private-user-images\.githubusercontent\.com/[^"'\s<>]+"#)
        .expect("pattern is valid")
});
static BARE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^[ \t]*(https://github\.com/user-attachments/assets/[A-Za-z0-9-]+|https?://\S+\.(?:mp4|mov|webm))[ \t]*$",
    )
    .expect("pattern is valid")
});

/// A description ready for layout: every image and video is a markdown image.
pub struct Prepared {
    pub markdown: String,
    /// Sizes the HTML gave (`width`, `height`), so space is held before the pixels arrive.
    pub hints: HashMap<String, (u32, u32)>,
    pub videos: HashSet<String>,
}

fn attrs(tag: &str) -> HashMap<String, String> {
    ATTR.captures_iter(tag)
        .map(|c| (c[1].to_ascii_lowercase(), c[2].to_owned()))
        .collect()
}

fn figure(alt: &str, src: &str) -> String {
    let alt: String = alt.chars().filter(|c| !matches!(c, '[' | ']')).collect();
    format!("\n\n![{alt}]({})\n\n", src.replace(' ', "%20"))
}

fn has_media(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("<img") || lower.contains("<video") || text.contains("![")
}

fn table_cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').unwrap_or(row);
    row.split('|').map(|cell| cell.trim().to_owned()).collect()
}

fn is_table_rule(line: &str) -> bool {
    let line = line.trim();
    line.starts_with('|')
        && line.contains('-')
        && line.chars().all(|c| matches!(c, '|' | ':' | '-' | ' '))
}

/// Pictures can't sit in a table's cells, so rows that hold them come out of
/// the table: each picture stacks under its column's heading ("Before",
/// "After"). Rows of plain text stay a table.
fn untable(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let is_row = |line: &str| line.trim_start().starts_with('|');
    let mut out: Vec<String> = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        if !(is_row(lines[at]) && lines.get(at + 1).is_some_and(|l| is_table_rule(l))) {
            out.push(lines[at].to_owned());
            at += 1;
            continue;
        }
        let mut end = at + 2;
        while end < lines.len() && is_row(lines[end]) {
            end += 1;
        }
        let (media, text): (Vec<&str>, Vec<&str>) =
            lines[at + 2..end].iter().partition(|row| has_media(row));
        if media.is_empty() {
            out.extend(lines[at..end].iter().map(|l| (*l).to_owned()));
        } else {
            if !text.is_empty() {
                out.push(lines[at].to_owned());
                out.push(lines[at + 1].to_owned());
                out.extend(text.iter().map(|l| (*l).to_owned()));
            }
            let headings = table_cells(lines[at]);
            for row in media {
                for (column, cell) in table_cells(row).into_iter().enumerate() {
                    if cell.is_empty() {
                        continue;
                    }
                    if let Some(heading) = headings.get(column).filter(|h| !h.is_empty()) {
                        out.push(format!("\n**{heading}**\n"));
                    }
                    out.push(format!("\n{cell}\n"));
                }
            }
        }
        at = end;
    }
    out.join("\n")
}

pub fn prepare(body: &str) -> Prepared {
    let mut hints = HashMap::new();
    let mut videos = HashSet::new();
    let body = untable(body);
    let text = VIDEO.replace_all(&body, |c: &Captures| {
        let found = attrs(&c[0]);
        match found.get("src") {
            Some(src) => {
                videos.insert(src.clone());
                figure("video", src)
            }
            None => String::new(),
        }
    });
    let text = IMG.replace_all(&text, |c: &Captures| {
        let found = attrs(&c[0]);
        let Some(src) = found.get("src") else {
            return String::new();
        };
        let size = |key: &str| found.get(key).and_then(|v| v.trim().parse::<u32>().ok());
        if let (Some(w), Some(h)) = (size("width"), size("height"))
            && w > 0
            && h > 0
        {
            hints.insert(src.clone(), (w, h));
        }
        figure(found.get("alt").map_or("image", String::as_str), src)
    });
    let text = BARE.replace_all(&text, |c: &Captures| figure("attachment", &c[1]));
    Prepared {
        markdown: text.into_owned(),
        hints,
        videos,
    }
}

/// What a link turned out to be, ready to draw.
enum Slot {
    Loading,
    Image(RgbaImage),
    /// A GIF with more than one frame, which plays on its own and loops.
    Animation(Animation),
    /// A frame of the video under a play button, or a card with one.
    Video(RgbaImage),
    Failed,
}

/// A video started in place: its link being signed, then playing.
enum Play {
    Signing {
        rx: Receiver<String>,
        size: (u32, u32),
    },
    Running(Playback),
}

/// An animated GIF's frames and how long each shows.
struct Animation {
    frames: Vec<(RgbaImage, Duration)>,
    length: Duration,
    /// The frames' bytes, taken from the gallery's GIF budget until dropped.
    _held: Held,
}

/// Bytes taken from a shared budget, given back when dropped.
struct Held {
    bytes: usize,
    budget: Arc<AtomicUsize>,
}

impl Held {
    fn none(budget: &Arc<AtomicUsize>) -> Self {
        Held {
            bytes: 0,
            budget: budget.clone(),
        }
    }

    /// Takes `bytes` more when the budget has them.
    fn take(&mut self, bytes: usize) -> bool {
        let taken = self
            .budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(bytes)
            })
            .is_ok();
        if taken {
            self.bytes += bytes;
        }
        taken
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.budget.fetch_add(self.bytes, Ordering::AcqRel);
    }
}

impl Animation {
    /// `None` unless there are at least two frames to play.
    fn new(frames: Vec<(RgbaImage, Duration)>, held: Held) -> Option<Self> {
        if frames.len() < 2 {
            return None;
        }
        let length = frames.iter().map(|(_, delay)| *delay).sum();
        Some(Animation {
            frames,
            length,
            _held: held,
        })
    }

    /// The frame showing `elapsed` into the loop, its index, and how long it shows on.
    fn at(&self, elapsed: Duration) -> (usize, &RgbaImage, Duration) {
        let mut into = elapsed.as_millis() % self.length.as_millis().max(1);
        for (index, (frame, delay)) in self.frames.iter().enumerate() {
            if into < delay.as_millis() {
                let left = Duration::from_millis((delay.as_millis() - into) as u64);
                return (index, frame, left);
            }
            into -= delay.as_millis();
        }
        (0, &self.frames[0].0, self.frames[0].1)
    }

    fn first(&self) -> &RgbaImage {
        &self.frames[0].0
    }
}

/// What's known of one figure, for what to show in its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    Loading,
    Image,
    Video,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Upload {
    href: String,
    cols: u16,
    from: u16,
    to: u16,
    /// What is drawn: 0 for a still picture, else a video frame, a GIF frame
    /// or a step of the loading ring.
    frame: Frame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Frame {
    Still,
    Video(u64),
    Gif(usize),
    /// The poster under a loading ring, at this step of its turn.
    Buffering(u64),
}

/// The description's media, and what of it the terminal holds.
pub struct Gallery {
    /// `None` when the terminal can't show Kitty graphics, or runs in tmux.
    kitty: Option<Kitty>,
    /// A cell's size in pixels.
    cell: (u16, u16),
    slots: HashMap<String, Slot>,
    hints: HashMap<String, (u32, u32)>,
    videos: HashSet<String>,
    /// Signed links to uploads, by upload id, from the rendered body, and when
    /// they were taken: they expire after `SIGNED_LINKS_LAST`.
    signed: HashMap<String, (String, Instant)>,
    /// The fetch each figure is waiting on; a result from any other is stale.
    requests: HashMap<String, u64>,
    next_request: u64,
    /// Bytes of GIF frames the gallery may still decode, shared by its fetches.
    gif_budget: Arc<AtomicUsize>,
    tx: Sender<(String, u64, Slot)>,
    rx: Receiver<(String, u64, Slot)>,
    uploads: HashMap<Upload, kitty::Image>,
    used: HashSet<Upload>,
    placements: Vec<(kitty::Image, Rect)>,
    /// The video playing in place, and its link.
    playing: Option<(String, Play)>,
    /// When the gallery was made: GIFs and the loading ring run on this clock.
    epoch: Instant,
    /// When something placed last frame changes next (a GIF's frame, the
    /// loading ring's step), so the screen is redrawn then and not sooner.
    next_change: Option<Duration>,
    /// Bumped when media arrives or a video starts or stops, so layouts and labels are redone.
    pub generation: u64,
}

impl Gallery {
    fn new(kitty: bool, cell: (u16, u16)) -> Self {
        let (tx, rx) = mpsc::channel();
        Gallery {
            kitty: kitty.then(Kitty::default),
            cell,
            slots: HashMap::new(),
            hints: HashMap::new(),
            videos: HashSet::new(),
            signed: HashMap::new(),
            requests: HashMap::new(),
            next_request: 0,
            gif_budget: Arc::new(AtomicUsize::new(GALLERY_GIF_BYTES)),
            tx,
            rx,
            uploads: HashMap::new(),
            used: HashSet::new(),
            placements: Vec::new(),
            playing: None,
            epoch: Instant::now(),
            next_change: None,
            generation: 0,
        }
    }

    /// Plays a video in its figure, `width` pixels across.
    /// The loading ring shows at once, while the link is signed and the
    /// first frame arrives; signing goes over the network, so it's done aside.
    pub fn play(&mut self, href: &str, width: u32) -> Result<(), PlayFailed> {
        let native = match self.slots.get(href) {
            Some(Slot::Video(poster)) => poster.dimensions(),
            _ => CARD,
        };
        self.stop();
        // Known now, so the caller can open the browser instead.
        if !video::has_ffmpeg() {
            return Err(PlayFailed::NoFfmpeg);
        }
        let size = video::decode_size(native, width);
        let play = match self.presigned(href) {
            Some(source) => Play::Running(Playback::start(&source, size)?),
            None => {
                let (tx, rx) = mpsc::channel();
                let href = href.to_owned();
                std::thread::spawn(move || {
                    let _ = tx.send(signed(&href));
                });
                Play::Signing { rx, size }
            }
        };
        self.playing = Some((href.to_owned(), play));
        self.generation += 1;
        Ok(())
    }

    /// Stops the video playing, leaving its poster.
    pub fn stop(&mut self) {
        if self.playing.take().is_some() {
            self.generation += 1;
        }
    }

    pub fn is_playing(&self, href: &str) -> bool {
        self.playing.as_ref().is_some_and(|(at, _)| at == href)
    }

    pub fn any_playing(&self) -> bool {
        self.playing.is_some()
    }

    /// How soon the screen should be drawn again for what's moving on it: a
    /// playing video's next frame, a GIF's next frame, or the loading ring's
    /// next step. `None` when nothing on screen moves.
    pub fn next_redraw(&self) -> Option<Duration> {
        // A video starting or playing is polled often, for its link and its frames.
        let video = self.playing.is_some().then_some(VIDEO_REDRAW);
        match (video, self.next_change) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether the video at `href` was started and has no frame to show yet.
    pub fn is_buffering(&self, href: &str) -> bool {
        match &self.playing {
            Some((at, play)) if at == href => match play {
                Play::Signing { .. } => true,
                Play::Running(playback) => playback.current().is_none(),
            },
            _ => false,
        }
    }

    /// Takes the playing video's newest frame, and stops it when it has
    /// ended. Says why when it ended before showing anything.
    pub fn tick(&mut self) -> Option<String> {
        let Some((_, play)) = &mut self.playing else {
            return None;
        };
        if let Play::Signing { rx, size } = play {
            let source = match rx.try_recv() {
                Ok(source) => source,
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => {
                    self.stop();
                    return Some(UNPLAYABLE.to_owned());
                }
            };
            match Playback::start(&source, *size) {
                Ok(playback) => *play = Play::Running(playback),
                Err(e) => {
                    self.stop();
                    return Some(e.to_string());
                }
            }
        }
        let Play::Running(playback) = play else {
            return None;
        };
        // The first frame turns the loading ring into the video, and its label too.
        if playback.poll() && playback.frame == 1 {
            self.generation += 1;
        }
        if !playback.ended() {
            return None;
        }
        let never_played = playback.frame == 0;
        self.stop();
        never_played.then(|| UNPLAYABLE.to_owned())
    }

    /// Asks the terminal what it can draw. Call it once the terminal is in raw mode.
    pub fn detect() -> Self {
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        let kitty = picker.protocol_type() == ProtocolType::Kitty && !picker.tmux_detected();
        let font = picker.font_size();
        Gallery::new(kitty, (font.width.max(1), font.height.max(1)))
    }

    #[cfg(test)]
    pub fn plain() -> Self {
        Gallery::new(false, (8, 16))
    }

    /// Whether images are drawn; without Kitty graphics they're placeholders.
    pub fn shows_media(&self) -> bool {
        self.kitty.is_some()
    }

    pub fn cell(&self) -> (u16, u16) {
        self.cell
    }

    pub fn learn(&mut self, prepared: &Prepared) {
        self.hints
            .extend(prepared.hints.iter().map(|(k, v)| (k.clone(), *v)));
        self.videos.extend(prepared.videos.iter().cloned());
    }

    /// The size to hold for a figure: what the HTML asked for, else the pixels' own.
    pub fn size(&self, href: &str) -> Option<(u32, u32)> {
        if let Some(size) = self.hints.get(href) {
            return Some(*size);
        }
        match self.slots.get(href)? {
            Slot::Image(pixels) | Slot::Video(pixels) => Some(pixels.dimensions()),
            Slot::Animation(animation) => Some(animation.first().dimensions()),
            Slot::Loading | Slot::Failed => None,
        }
    }

    pub fn shown(&self, href: &str) -> Shown {
        match self.slots.get(href) {
            None | Some(Slot::Loading) => Shown::Loading,
            Some(Slot::Image(_) | Slot::Animation(_)) => Shown::Image,
            Some(Slot::Video(_)) => Shown::Video,
            Some(Slot::Failed) => Shown::Failed,
        }
    }

    pub fn is_video(&self, href: &str) -> bool {
        self.videos.contains(href) || self.shown(href) == Shown::Video
    }

    /// Takes the rendered body's signed links. An upload that failed, or is
    /// still loading on an older link, is fetched again with the new one (the
    /// older fetch's result is dropped), so a reload recovers expired links.
    pub fn sign(&mut self, links: &HashMap<String, String>) {
        let now = Instant::now();
        let mut changed = Vec::new();
        for (id, link) in links {
            if self.signed.get(id).is_some_and(|(known, _)| known == link) {
                continue;
            }
            self.signed.insert(id.clone(), (link.clone(), now));
            changed.push(id.clone());
        }
        if changed.is_empty() {
            return;
        }
        let stale: Vec<String> = self
            .slots
            .iter()
            .filter(|(href, slot)| {
                matches!(slot, Slot::Failed | Slot::Loading)
                    && changed.iter().any(|id| href.contains(id.as_str()))
            })
            .map(|(href, _)| href.clone())
            .collect();
        for href in stale {
            self.slots.remove(&href);
            self.requests.remove(&href);
        }
        self.generation += 1;
    }

    /// The rendered body's signed link for a github.com upload, while it's fresh.
    fn presigned(&self, href: &str) -> Option<String> {
        if !href.starts_with("https://github.com/") {
            return None;
        }
        let id = UPLOAD_ID.find(href)?;
        self.signed
            .get(id.as_str())
            .filter(|(_, at)| at.elapsed() < SIGNED_LINKS_LAST)
            .map(|(link, _)| link.clone())
    }

    /// Starts fetching a figure the first time it's asked for.
    pub fn want(&mut self, href: &str) {
        if self.kitty.is_none() || self.slots.contains_key(href) {
            return;
        }
        self.slots.insert(href.to_owned(), Slot::Loading);
        self.next_request += 1;
        let request = self.next_request;
        self.requests.insert(href.to_owned(), request);
        let (tx, url, video) = (self.tx.clone(), href.to_owned(), self.videos.contains(href));
        let (presigned, budget) = (self.presigned(href), self.gif_budget.clone());
        std::thread::spawn(move || {
            let slot = fetch(&url, presigned, video, &budget);
            let _ = tx.send((url, request, slot));
        });
    }

    /// Takes in media that arrived; `true` when any did. A result from a
    /// fetch that was since replaced is dropped.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok((href, request, slot)) = self.rx.try_recv() {
            if self.requests.get(&href) != Some(&request) {
                continue;
            }
            self.requests.remove(&href);
            self.slots.insert(href, slot);
            any = true;
        }
        if any {
            self.generation += 1;
        }
        any
    }

    pub fn loading(&self) -> bool {
        self.slots.values().any(|s| matches!(s, Slot::Loading))
    }

    /// Starts a frame's placements.
    pub fn begin(&mut self) {
        self.placements.clear();
        self.used.clear();
        self.next_change = None;
    }

    /// Places the part of `figure` that's on screen. `area` is where the body
    /// is drawn, showing its rows from `scroll`.
    pub fn place(&mut self, figure: &Figure, area: Rect, scroll: usize) {
        let Some(kitty) = &mut self.kitty else { return };
        let elapsed = self.epoch.elapsed();
        // A playing video shows its newest frame over the poster, and its
        // poster under a loading ring until the first frame comes.
        let playing = self
            .playing
            .as_ref()
            .filter(|(at, _)| *at == figure.href)
            .map(|(_, play)| match play {
                Play::Running(playback) => {
                    playback.current().map(|pixels| (pixels, playback.frame))
                }
                Play::Signing { .. } => None,
            });
        // The pixels to show, and which frame they are; the loading ring is
        // drawn only when its step isn't uploaded already.
        let (source, frame) = match (playing, self.slots.get(&figure.href)) {
            (Some(Some((pixels, frame))), _) => (pixels, Frame::Video(frame)),
            (Some(None), Some(Slot::Video(poster))) => {
                let ticks = elapsed.as_millis() / RING_STEP.as_millis();
                let into_step = elapsed.as_millis() % RING_STEP.as_millis();
                sooner(
                    &mut self.next_change,
                    RING_STEP.saturating_sub(Duration::from_millis(into_step as u64)),
                );
                (poster, Frame::Buffering(ticks as u64 % RING_STEPS))
            }
            (_, Some(Slot::Animation(animation))) => {
                let (index, pixels, left) = animation.at(elapsed);
                sooner(&mut self.next_change, left);
                (pixels, Frame::Gif(index))
            }
            (_, Some(Slot::Image(pixels) | Slot::Video(pixels))) => (pixels, Frame::Still),
            _ => return,
        };
        let (top, bottom) = (figure.top, figure.top + figure.rows as usize);
        let (first, last) = (scroll, scroll + area.height as usize);
        if bottom <= first || top >= last {
            return;
        }
        let from = (top.max(first) - top) as u16;
        let to = (bottom.min(last) - top) as u16;
        let key = Upload {
            href: figure.href.clone(),
            cols: figure.cols,
            from,
            to,
            frame,
        };
        if !self.uploads.contains_key(&key) {
            let pixels = match frame {
                Frame::Buffering(step) => Cow::Owned(buffering(source, step)),
                Frame::Still | Frame::Video(_) | Frame::Gif(_) => Cow::Borrowed(source),
            };
            let pixels = pixels.as_ref();
            // Only the rows on screen are sent: a placement can't clip an image's top.
            let (w, h) = pixels.dimensions();
            let rows = u32::from(figure.rows.max(1));
            let y0 = h * u32::from(from) / rows;
            let y1 = (h * u32::from(to) / rows).max(y0 + 1).min(h);
            let slice = image::imageops::crop_imm(pixels, 0, y0, w, y1 - y0).to_image();
            // Placements never scale up, so the slice is sized to fill its cells first.
            let (cw, ch) = (f32::from(self.cell.0), f32::from(self.cell.1));
            let (box_w, box_h) = (f32::from(figure.cols) * cw, f32::from(to - from) * ch);
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
            let cells = |px: u32, cell: u16, max: u16| {
                (px.div_ceil(u32::from(cell.max(1))) as u16).clamp(1, max)
            };
            let cols = cells(slice.width(), self.cell.0, figure.cols);
            let rows = cells(slice.height(), self.cell.1, to - from);
            let Some(uploaded) = kitty.upload(&slice, cols, rows, 0) else {
                return;
            };
            self.uploads.insert(key.clone(), uploaded);
        }
        let Some(image) = self.uploads.get(&key).copied() else {
            return;
        };
        let x = area.x + figure.x + figure.cols.saturating_sub(image.cols) / 2;
        let visible = to - from;
        let start = area.y + (top + from as usize - scroll) as u16;
        // A figure cut off at the top sits against the bottom of its visible rows.
        let y = if from > 0 {
            start + visible - image.rows.min(visible)
        } else {
            start
        };
        self.placements
            .push((image, Rect::new(x, y, image.cols, image.rows.min(visible))));
        self.used.insert(key);
    }

    /// Frees uploads this frame didn't use, then puts this frame's on screen.
    pub fn flush(&mut self) -> io::Result<()> {
        let Some(kitty) = &mut self.kitty else {
            return Ok(());
        };
        let used = &self.used;
        self.uploads.retain(|key, image| {
            let kept = used.contains(key);
            if !kept {
                kitty.free(*image);
            }
            kept
        });
        kitty.flush(&mut io::stdout(), &self.placements)
    }

    /// Takes every image off the screen, as before handing the terminal over.
    pub fn clear(&mut self) -> io::Result<()> {
        self.placements.clear();
        self.used.clear();
        self.flush()
    }

    /// The screen was cleared: place everything again on the next flush.
    pub fn reset(&mut self) {
        if let Some(kitty) = &mut self.kitty {
            kitty.reset();
        }
        self.uploads.clear();
    }
}

fn is_video_file(url: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    [".mp4", ".mov", ".webm", ".m4v"]
        .iter()
        .any(|ext| path.ends_with(ext))
}

/// FNV-1a, to name a link's file in the cache.
fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Where a link's picture is kept: an image's own bytes, or a video's poster as PNG.
fn kept(url: &str, video: bool) -> Option<std::path::PathBuf> {
    let kind = if video { "video" } else { "image" };
    crate::pr::cache_file(&format!("media-{:016x}.{kind}", hash(url)))
}

/// Saves bytes for next time; a cache that can't be written only means fetching again.
fn keep(path: Option<std::path::PathBuf>, bytes: &[u8]) {
    let Some(path) = path else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let partial = path.with_extension(format!("{}.part", std::process::id()));
    if std::fs::write(&partial, bytes).is_ok() && std::fs::rename(&partial, &path).is_err() {
        let _ = std::fs::remove_file(&partial);
    }
}

/// A link's picture: from the cache when it was fetched before, which links
/// to uploads allow, since an upload never changes. `presigned` is the
/// rendered body's signed link for it, when it had one.
fn fetch(url: &str, presigned: Option<String>, video: bool, budget: &Arc<AtomicUsize>) -> Slot {
    for (is_video, path) in [(true, kept(url, true)), (false, kept(url, false))] {
        let Some(bytes) = path.and_then(|p| std::fs::read(p).ok()) else {
            continue;
        };
        let slot = if is_video {
            decode(&bytes).map(Slot::Video)
        } else {
            picture(&bytes, budget)
        };
        if let Some(slot) = slot {
            return slot;
        }
    }
    let source = presigned.unwrap_or_else(|| signed(url));
    let slot = download(url, &source, video, budget);
    match &slot {
        Slot::Video(pixels) => {
            let mut png = Vec::new();
            if image::DynamicImage::ImageRgba8(pixels.clone())
                .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
                .is_ok()
            {
                keep(kept(url, true), &png);
            }
        }
        Slot::Image(_) | Slot::Animation(_) | Slot::Loading | Slot::Failed => {}
    }
    slot
}

/// The GitHub token, from GH_TOKEN, GITHUB_TOKEN or `gh auth token`.
static TOKEN: LazyLock<Option<String>> = LazyLock::new(|| {
    let from_env = ["GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok());
    let token = from_env.or_else(|| {
        let output = Command::new("gh")
            .args(["auth", "token"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        String::from_utf8(output.stdout).ok()
    })?;
    Some(token.trim().to_owned()).filter(|t| !t.is_empty())
});

/// Uploads to a private repository answer 404 without sign-in. With the
/// GitHub token, github.com redirects to a signed link that works for a few
/// minutes, so that's what is fetched. The token goes only to github.com;
/// any other link, or one that doesn't redirect, is used as it is.
fn signed(url: &str) -> String {
    let Some(token) = TOKEN
        .as_deref()
        .filter(|_| url.starts_with("https://github.com/"))
    else {
        return url.to_owned();
    };
    let response = ureq::get(url)
        .config()
        .max_redirects(0)
        .build()
        .header("user-agent", USER_AGENT)
        .header("authorization", &format!("Bearer {token}"))
        .call();
    response
        .ok()
        .filter(|r| r.status().is_redirection())
        .and_then(|r| {
            r.headers()
                .get("location")?
                .to_str()
                .ok()
                .map(str::to_owned)
        })
        .filter(|location| location.starts_with("https://"))
        .unwrap_or_else(|| url.to_owned())
}

/// The signed links in a pull request's rendered body (`body_html`), by
/// upload id. github.com signs uploads only for a browser session where SAML
/// SSO is enforced, but these links download without any token.
pub fn signed_links(html: &str) -> HashMap<String, String> {
    SIGNED
        .find_iter(html)
        .filter_map(|link| {
            let link = link.as_str().replace("&amp;", "&");
            let id = UPLOAD_ID.find(&link)?.as_str().to_owned();
            Some((id, link))
        })
        .collect()
}

/// Fetches `url` (the link itself, or a signed link to it), keeping an image
/// under `key`, the link as written: a signed link changes every time.
fn download(key: &str, url: &str, video: bool, budget: &Arc<AtomicUsize>) -> Slot {
    if video || is_video_file(url) {
        return Slot::Video(playable(poster(url)));
    }
    let Ok(response) = ureq::get(url).header("user-agent", USER_AGENT).call() else {
        return Slot::Failed;
    };
    let kind = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if kind.starts_with("video/") {
        drop(response);
        return Slot::Video(playable(poster(url)));
    }
    let Ok(bytes) = response
        .into_body()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()
    else {
        return Slot::Failed;
    };
    match picture(&bytes, budget) {
        Some(slot) => {
            keep(kept(key, false), &bytes);
            slot
        }
        None => Slot::Failed,
    }
}

/// A picture ready to draw: a GIF with more than one frame plays, any other
/// picture is still.
fn picture(bytes: &[u8], budget: &Arc<AtomicUsize>) -> Option<Slot> {
    if bytes.starts_with(b"GIF8")
        && let Some(animation) = animation(bytes, budget)
    {
        return Some(Slot::Animation(animation));
    }
    decode(bytes).map(Slot::Image)
}

/// A GIF's frames, composed as they show; `None` for one frame, which is
/// drawn as a still picture. Frames come out of the gallery's shared budget
/// and stop at this GIF's own limit; a GIF that gets fewer than two plays
/// as its first frame.
fn animation(bytes: &[u8], budget: &Arc<AtomicUsize>) -> Option<Animation> {
    use image::AnimationDecoder;

    use image::ImageDecoder;

    let mut decoder = image::codecs::gif::GifDecoder::new(io::Cursor::new(bytes)).ok()?;
    // Caps what decoding itself allocates (the canvas a GIF declares can be
    // 65535 × 65535): nothing larger than one GIF's share could be kept anyway.
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_GIF_BYTES as u64);
    decoder.set_limits(limits).ok()?;
    let mut frames = Vec::new();
    let mut held = Held::none(budget);
    for frame in decoder.into_frames().take(MAX_GIF_FRAMES) {
        let Ok(frame) = frame else { break };
        let delay = Duration::from(frame.delay());
        let delay = if delay < GIF_MIN_DELAY {
            GIF_DEFAULT_DELAY
        } else {
            delay
        };
        let pixels = frame.into_buffer();
        let size = pixels.as_raw().len();
        if held.bytes + size > MAX_GIF_BYTES || !held.take(size) {
            break;
        }
        frames.push((pixels, delay));
    }
    Animation::new(frames, held)
}

/// A picture's pixels: any format the image crate reads, or an SVG drawn at
/// its own size.
fn decode(bytes: &[u8]) -> Option<RgbaImage> {
    if let Ok(image) = image::load_from_memory(bytes) {
        return Some(image.to_rgba8());
    }
    let options = usvg::Options {
        fontdb: crate::mermaid::FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).ok()?;
    let size = tree.size();
    let scale = (MAX_SVG_SIDE / size.width().max(size.height())).min(1.0);
    let width = (size.width() * scale).ceil() as u32;
    let height = (size.height() * scale).ceil() as u32;
    let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let pixels = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    });
    RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
}

/// A frame from early in the video, taken with ffmpeg when it's installed.
fn poster(url: &str) -> Option<RgbaImage> {
    ["0.5", "0"].iter().find_map(|at| {
        let output = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-user_agent",
                USER_AGENT,
                "-ss",
                at,
                "-i",
                url,
                "-frames:v",
                "1",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "-",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        image::load_from_memory(&output.stdout)
            .ok()
            .map(|image| image.to_rgba8())
    })
}

/// The frame, or a dark card, with a play button in the middle.
fn playable(frame: Option<RgbaImage>) -> RgbaImage {
    let mut image =
        frame.unwrap_or_else(|| RgbaImage::from_pixel(CARD.0, CARD.1, Rgba([24, 24, 24, 255])));
    let (w, h) = (image.width() as f32, image.height() as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let radius = (w.min(h) / 7.0).max(12.0);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        if dx * dx + dy * dy > radius * radius {
            continue;
        }
        // A triangle pointing right, inside the circle.
        let (tx, ty) = (dx / radius, dy / radius);
        let inside = (-0.3..=0.5).contains(&tx) && ty.abs() <= (0.5 - tx) * 0.72;
        let Rgba([r, g, b, _]) = *pixel;
        *pixel = if inside {
            Rgba([255, 255, 255, 255])
        } else {
            let dim = |c: u8| (u16::from(c) * 2 / 5) as u8;
            Rgba([dim(r), dim(g), dim(b), 255])
        };
    }
    image
}

/// Keeps the sooner of two deadlines.
fn sooner(known: &mut Option<Duration>, after: Duration) {
    *known = Some(known.map_or(after, |known| known.min(after)));
}

/// The poster with a loading ring where its play button was, the ring's
/// bright head at `step` of `RING_STEPS` round.
fn buffering(poster: &RgbaImage, step: u64) -> RgbaImage {
    let mut image = poster.clone();
    let (w, h) = (image.width() as f32, image.height() as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let radius = (w.min(h) / 7.0).max(12.0);
    let head = step as f32 / RING_STEPS as f32 * std::f32::consts::TAU;
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
        let distance = (dx * dx + dy * dy).sqrt();
        if distance > radius {
            continue;
        }
        // Covers the play button, then draws a ring that fades behind its head.
        let mut shade = Rgba([24, 24, 24, 255]);
        if (0.5..=0.75).contains(&(distance / radius)) {
            let angle = dy.atan2(dx).rem_euclid(std::f32::consts::TAU);
            let behind = (head - angle).rem_euclid(std::f32::consts::TAU);
            let lit = (1.0 - behind / (std::f32::consts::TAU * 0.75)).max(0.0);
            let mix = |c: u8| (f32::from(c) + (255.0 - f32::from(c)) * lit) as u8;
            shade = Rgba([mix(shade[0]), mix(shade[1]), mix(shade[2]), 255]);
        }
        *pixel = shade;
    }
    image
}

/// Opens a link in the browser or the system's player.
pub fn open(url: &str) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    Command::new(opener)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_media_becomes_markdown_figures_with_their_sizes() {
        let body = "Before\n<img width=\"663\" height=\"200\" alt=\"shot\" src=\"https://x/a.png\" />\n\
            <video src=\"https://x/v.mp4\" controls></video>\n\
            https://github.com/user-attachments/assets/0ae4-be5e\n\
            ![md](https://x/b.png)";
        let prepared = prepare(body);
        assert!(prepared.markdown.contains("![shot](https://x/a.png)"));
        assert!(prepared.markdown.contains("![video](https://x/v.mp4)"));
        assert!(
            prepared
                .markdown
                .contains("![attachment](https://github.com/user-attachments/assets/0ae4-be5e)")
        );
        assert!(prepared.markdown.contains("![md](https://x/b.png)"));
        assert_eq!(prepared.hints.get("https://x/a.png"), Some(&(663, 200)));
        assert!(prepared.videos.contains("https://x/v.mp4"));
    }

    #[test]
    fn pictures_in_a_table_stack_under_their_headings() {
        let body = "| Before | After |\n| --- | --- |\n| <img src=\"https://x/a.png\"> | <img src=\"https://x/b.png\"> |\n| text | more |";
        let prepared = prepare(body);
        let md = &prepared.markdown;
        let (before, a, after, b) = (
            md.find("**Before**").unwrap(),
            md.find("(https://x/a.png)").unwrap(),
            md.find("**After**").unwrap(),
            md.find("(https://x/b.png)").unwrap(),
        );
        assert!(before < a && a < after && after < b, "{md}");
        assert!(
            md.contains("| text | more |"),
            "plain rows stay a table: {md}"
        );
        assert_eq!(
            untable("| a | b |\n|---|---|\n| 1 | 2 |"),
            "| a | b |\n|---|---|\n| 1 | 2 |"
        );
    }

    #[test]
    fn a_link_inside_a_sentence_stays_a_link() {
        let prepared = prepare("See https://github.com/user-attachments/assets/abc for it");
        assert!(!prepared.markdown.contains("!["));
    }

    #[test]
    fn videos_get_a_play_button_even_without_a_frame() {
        let card = playable(None);
        assert_eq!(card.dimensions(), CARD);
        assert_eq!(
            card.get_pixel(CARD.0 / 2, CARD.1 / 2),
            &Rgba([255, 255, 255, 255])
        );
        assert_eq!(card.get_pixel(2, 2), &Rgba([24, 24, 24, 255]));
        assert!(is_video_file("https://x/a.MP4?raw=1"));
        assert!(!is_video_file("https://x/a.png"));
    }

    #[test]
    fn only_github_links_are_signed() {
        assert_eq!(signed("https://x/a.png"), "https://x/a.png");
        assert_eq!(
            signed("http://github.com/user-attachments/assets/a"),
            "http://github.com/user-attachments/assets/a"
        );
    }

    #[test]
    fn svgs_are_drawn_at_their_own_size() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="#ff0000"/></svg>"##;
        let pixels = decode(svg).expect("an SVG decodes");
        assert_eq!(pixels.dimensions(), (40, 20));
        assert_eq!(pixels.get_pixel(20, 10), &Rgba([255, 0, 0, 255]));
        assert!(decode(b"not a picture").is_none());
    }

    #[test]
    fn gifs_with_frames_play_on_their_delays_and_loop() {
        let frame = |c: u8, ms: u32| {
            image::Frame::from_parts(
                RgbaImage::from_pixel(4, 2, Rgba([c, 0, 0, 255])),
                0,
                0,
                image::Delay::from_numer_denom_ms(ms, 1),
            )
        };
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .encode_frames([frame(255, 100), frame(0, 300)])
                .expect("frames encode");
        }
        let budget = Arc::new(AtomicUsize::new(GALLERY_GIF_BYTES));
        let Some(Slot::Animation(animation)) = picture(&gif, &budget) else {
            panic!("a GIF with two frames animates");
        };
        assert_eq!(animation.length, Duration::from_millis(400));
        let at = |ms| {
            let (index, _, left) = animation.at(Duration::from_millis(ms));
            (index, left.as_millis())
        };
        assert_eq!(at(50), (0, 50));
        assert_eq!(at(150), (1, 250));
        assert_eq!(at(450), (0, 50));
        // Its frames (two of 4×2 RGBA) are held from the budget until it's dropped.
        assert_eq!(budget.load(Ordering::Acquire), GALLERY_GIF_BYTES - 64);
        drop(animation);
        assert_eq!(budget.load(Ordering::Acquire), GALLERY_GIF_BYTES);

        // With the budget spent, a GIF shows its first frame, still.
        let spent = Arc::new(AtomicUsize::new(40));
        assert!(matches!(picture(&gif, &spent), Some(Slot::Image(_))));
        assert_eq!(
            spent.load(Ordering::Acquire),
            40,
            "what it took is given back"
        );

        let mut still = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut still);
            encoder
                .encode_frames([frame(255, 100)])
                .expect("frame encodes");
        }
        assert!(matches!(picture(&still, &budget), Some(Slot::Image(_))));
    }

    #[test]
    fn a_loading_video_covers_its_play_button_with_a_ring() {
        let poster = playable(None);
        let ring = buffering(&poster, 0);
        let (cx, cy) = (CARD.0 / 2, CARD.1 / 2);
        assert_eq!(poster.get_pixel(cx, cy), &Rgba([255, 255, 255, 255]));
        assert_eq!(ring.get_pixel(cx, cy), &Rgba([24, 24, 24, 255]));
        assert_eq!(ring.get_pixel(2, 2), poster.get_pixel(2, 2));
        assert_ne!(buffering(&poster, 3), ring, "the ring turns");
    }

    const ID: &str = "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0";

    #[test]
    fn rendered_body_links_are_found_by_upload_id() {
        let html = format!(
            r#"<p><a href="https://private-user-images.githubusercontent.com/12/412345678-{ID}.png?jwt=a.b&amp;x=1"><img src="https://private-user-images.githubusercontent.com/12/412345678-{ID}.png?jwt=a.b&amp;x=1" alt="shot"></a> <img src="https://x/other.png"></p>"#
        );
        let links = signed_links(&html);
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[ID],
            format!(
                "https://private-user-images.githubusercontent.com/12/412345678-{ID}.png?jwt=a.b&x=1"
            )
        );
    }

    #[test]
    fn uploads_use_the_rendered_body_link_and_failures_are_retried() {
        let upload = format!("https://github.com/user-attachments/assets/{ID}");
        let mut gallery = Gallery::plain();
        assert_eq!(gallery.presigned(&upload), None);
        gallery.slots.insert(upload.clone(), Slot::Failed);
        gallery.slots.insert("https://x/a.png".into(), Slot::Failed);

        let link = format!("https://private-user-images.githubusercontent.com/1/2-{ID}.png?jwt=t");
        gallery.sign(&HashMap::from([(ID.to_owned(), link.clone())]));
        assert_eq!(gallery.presigned(&upload), Some(link.clone()));
        assert_eq!(gallery.presigned(&format!("https://x/{ID}.png")), None);
        // The upload that failed is fetched again; other failures stay.
        assert_eq!(gallery.shown(&upload), Shown::Loading);
        assert!(!gallery.slots.contains_key(&upload));
        assert_eq!(gallery.shown("https://x/a.png"), Shown::Failed);

        // The same links again change nothing.
        let generation = gallery.generation;
        gallery.sign(&HashMap::from([(ID.to_owned(), link.clone())]));
        assert_eq!(gallery.generation, generation);

        // An expired link isn't used; the upload is signed through github.com instead.
        let stale = Instant::now().checked_sub(SIGNED_LINKS_LAST);
        if let Some(stale) = stale {
            gallery.signed.insert(ID.to_owned(), (link, stale));
            assert_eq!(gallery.presigned(&upload), None);
        }
    }

    #[test]
    fn a_fetch_replaced_by_a_new_link_is_dropped_when_it_lands() {
        let upload = format!("https://github.com/user-attachments/assets/{ID}");
        let mut gallery = Gallery::plain();
        // A fetch on the old link is under way.
        gallery.slots.insert(upload.clone(), Slot::Loading);
        gallery.requests.insert(upload.clone(), 1);

        let link = format!("https://private-user-images.githubusercontent.com/1/2-{ID}.png?jwt=t");
        gallery.sign(&HashMap::from([(ID.to_owned(), link)]));
        assert!(
            !gallery.slots.contains_key(&upload),
            "fetched again with the new link"
        );

        // The old fetch then fails; its result is stale and dropped.
        gallery
            .tx
            .send((upload.clone(), 1, Slot::Failed))
            .expect("channel open");
        assert!(!gallery.receive());
        assert_eq!(gallery.shown(&upload), Shown::Loading);

        // The current fetch's result is taken.
        gallery.requests.insert(upload.clone(), 2);
        gallery
            .tx
            .send((upload.clone(), 2, Slot::Image(RgbaImage::new(1, 1))))
            .expect("channel open");
        assert!(gallery.receive());
        assert_eq!(gallery.shown(&upload), Shown::Image);
    }

    #[test]
    fn without_kitty_nothing_is_fetched() {
        let mut gallery = Gallery::plain();
        gallery.want("https://x/a.png");
        assert!(!gallery.loading());
        assert!(!gallery.shows_media());
    }
}
