//! Pictures in a pull request's description, drawn the way the reader draws
//! figures: straight onto the screen with the Kitty graphics protocol. Images
//! show as they are; a video shows its first frame (taken with ffmpeg when
//! it's installed) under a play button, and a click opens it. A terminal
//! without Kitty graphics keeps a one-line placeholder for each.

use std::collections::{HashMap, HashSet};
use std::io;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{self, Receiver, Sender};

use image::{Rgba, RgbaImage};
use ratatui::layout::Rect;
use ratatui_image::picker::{Picker, ProtocolType};
use regex::{Captures, Regex};

use crate::kitty::{self, Kitty};
use crate::render::Figure;
use crate::video::{self, PlayFailed, Playback};

const USER_AGENT: &str = "Mozilla/5.0 (leaf reader)";
/// An image larger than this is left as a placeholder rather than held in memory.
const MAX_BYTES: u64 = 64 * 1024 * 1024;
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
    /// A frame of the video under a play button, or a card with one.
    Video(RgbaImage),
    Failed,
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
    /// The video frame shown, or 0 for a still picture.
    frame: u64,
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
    tx: Sender<(String, Slot)>,
    rx: Receiver<(String, Slot)>,
    uploads: HashMap<Upload, kitty::Image>,
    used: HashSet<Upload>,
    placements: Vec<(kitty::Image, Rect)>,
    /// The video playing in place, and its link.
    playing: Option<(String, Playback)>,
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
            tx,
            rx,
            uploads: HashMap::new(),
            used: HashSet::new(),
            placements: Vec::new(),
            playing: None,
            generation: 0,
        }
    }

    /// Plays a video in its figure, `width` pixels across.
    pub fn play(&mut self, href: &str, width: u32) -> Result<(), PlayFailed> {
        let native = match self.slots.get(href) {
            Some(Slot::Video(poster)) => poster.dimensions(),
            _ => CARD,
        };
        self.stop();
        let playback = Playback::start(href, video::decode_size(native, width))?;
        self.playing = Some((href.to_owned(), playback));
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

    /// Takes the playing video's newest frame, and stops it when it has ended.
    pub fn tick(&mut self) {
        let ended = match &mut self.playing {
            Some((_, playback)) => {
                playback.poll();
                playback.ended()
            }
            None => false,
        };
        if ended {
            self.stop();
        }
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
            Slot::Loading | Slot::Failed => None,
        }
    }

    pub fn shown(&self, href: &str) -> Shown {
        match self.slots.get(href) {
            None | Some(Slot::Loading) => Shown::Loading,
            Some(Slot::Image(_)) => Shown::Image,
            Some(Slot::Video(_)) => Shown::Video,
            Some(Slot::Failed) => Shown::Failed,
        }
    }

    pub fn is_video(&self, href: &str) -> bool {
        self.videos.contains(href) || self.shown(href) == Shown::Video
    }

    /// Starts fetching a figure the first time it's asked for.
    pub fn want(&mut self, href: &str) {
        if self.kitty.is_none() || self.slots.contains_key(href) {
            return;
        }
        self.slots.insert(href.to_owned(), Slot::Loading);
        let (tx, url, video) = (self.tx.clone(), href.to_owned(), self.videos.contains(href));
        std::thread::spawn(move || {
            let slot = fetch(&url, video);
            let _ = tx.send((url, slot));
        });
    }

    /// Takes in media that arrived; `true` when any did.
    pub fn receive(&mut self) -> bool {
        let mut any = false;
        while let Ok((href, slot)) = self.rx.try_recv() {
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
    }

    /// Places the part of `figure` that's on screen. `area` is where the body
    /// is drawn, showing its rows from `scroll`.
    pub fn place(&mut self, figure: &Figure, area: Rect, scroll: usize) {
        let Some(kitty) = &mut self.kitty else { return };
        // A playing video shows its newest frame over the poster.
        let frame = self
            .playing
            .as_ref()
            .filter(|(at, _)| *at == figure.href)
            .and_then(|(_, playback)| Some((playback.current()?, playback.frame)));
        let (pixels, frame) = match (frame, self.slots.get(&figure.href)) {
            (Some((pixels, frame)), _) => (pixels, frame),
            (None, Some(Slot::Image(pixels) | Slot::Video(pixels))) => (pixels, 0),
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
/// to uploads allow, since an upload never changes.
fn fetch(url: &str, video: bool) -> Slot {
    for (is_video, path) in [(true, kept(url, true)), (false, kept(url, false))] {
        let Some(bytes) = path.and_then(|p| std::fs::read(p).ok()) else {
            continue;
        };
        if let Ok(image) = image::load_from_memory(&bytes) {
            let pixels = image.to_rgba8();
            return if is_video {
                Slot::Video(pixels)
            } else {
                Slot::Image(pixels)
            };
        }
    }
    let slot = download(url, video);
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
        Slot::Image(_) | Slot::Loading | Slot::Failed => {}
    }
    slot
}

fn download(url: &str, video: bool) -> Slot {
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
    match image::load_from_memory(&bytes) {
        Ok(image) => {
            keep(kept(url, false), &bytes);
            Slot::Image(image.to_rgba8())
        }
        Err(_) => Slot::Failed,
    }
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
    fn without_kitty_nothing_is_fetched() {
        let mut gallery = Gallery::plain();
        gallery.want("https://x/a.png");
        assert!(!gallery.loading());
        assert!(!gallery.shows_media());
    }
}
