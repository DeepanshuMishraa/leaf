//! A readable document: markdown pages plus the images they reference.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use image::RgbaImage;

use crate::mono::Rgb;

const IMAGE_SCHEME: &str = "leaf-image:";
pub const WATCH_TICK: Duration = Duration::from_millis(250);
/// How often a live file, like a board an agent draws on, is looked at.
pub const LIVE_TICK: Duration = Duration::from_millis(30);
const RETRY: Duration = Duration::from_secs(5);

type Stamp = (SystemTime, u64);

pub struct Watch {
    path: PathBuf,
    loaded: Option<Stamp>,
    seen: Option<Stamp>,
    checked: Instant,
    failed: Option<(Stamp, Instant)>,
    pending: Option<(Stamp, crate::pdf::Loading)>,
    tick: Duration,
    /// Whether a change waits for the file to look the same twice before
    /// it's read. A live file is read at once and a half-written one is
    /// refused by its loader instead.
    settle: bool,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl Watch {
    pub fn new(path: PathBuf) -> Self {
        let loaded = stamp(&path);
        Self {
            path,
            loaded,
            seen: loaded,
            checked: Instant::now(),
            failed: None,
            pending: None,
            tick: WATCH_TICK,
            settle: true,
        }
    }

    /// Watches a file that changes while it's open, picking up each write
    /// within `LIVE_TICK`.
    pub fn live(path: PathBuf) -> Self {
        Self {
            tick: LIVE_TICK,
            settle: false,
            ..Self::new(path)
        }
    }

    fn changed(&mut self) -> Option<Stamp> {
        if self.checked.elapsed() < self.tick {
            return None;
        }
        self.checked = Instant::now();
        let now = stamp(&self.path);
        let settled = !self.settle || now == self.seen;
        self.seen = now;
        if !settled || now.is_none() || now == self.loaded {
            return None;
        }
        if self
            .failed
            .is_some_and(|(at, failed)| Some(at) == now && failed.elapsed() < RETRY)
        {
            return None;
        }
        now
    }

    /// Notes a write of our own. The file is read again on the next look
    /// rather than taken as ours, since someone else may have written it
    /// right after; the reader drops it when it matches what it wrote.
    pub fn saved(&mut self) {
        self.loaded = None;
        self.failed = None;
    }

    pub fn read<T, E>(&mut self, load: impl FnOnce(&Path) -> Result<T, E>) -> Option<T> {
        let at = self.changed()?;
        let value = match load(&self.path) {
            Ok(value) => value,
            Err(_) => {
                self.failed = Some((at, Instant::now()));
                return None;
            }
        };
        if stamp(&self.path) != Some(at) {
            return None;
        }
        self.loaded = Some(at);
        self.failed = None;
        Some(value)
    }

    pub fn document(&mut self, load: fn(&Path) -> Result<Doc, String>) -> Option<Doc> {
        // Big office files take a moment to lay out, so they load in the
        // background like PDFs and EPUBs.
        let paged = self
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("pdf") || extension.eq_ignore_ascii_case("epub")
            })
            || crate::office::Kind::of(&self.path).is_some();
        if !paged {
            return self.read(load);
        }
        if let Some((at, loading)) = &self.pending {
            let result = loading.poll()?;
            let at = *at;
            self.pending = None;
            if stamp(&self.path) != Some(at) {
                return None;
            }
            return match result {
                Ok(doc) => {
                    self.loaded = Some(at);
                    self.failed = None;
                    Some(doc)
                }
                Err(_) => {
                    self.failed = Some((at, Instant::now()));
                    None
                }
            };
        }
        let at = self.changed()?;
        self.pending = Some((at, crate::pdf::Loading::with(self.path.clone(), load)));
        None
    }
}

/// Where a markdown image href points.
pub enum ImageRef<'a> {
    /// An image embedded in the document, by id.
    Embedded(&'a str),
    /// An image on the web.
    Remote(&'a str),
}

/// What a document is made of. Pages are read on demand, so a book of
/// thousands of pages opens at once and only the pages being read cost
/// anything. Embedded images are decoded only when shown.
pub trait Source {
    /// Markdown of the page at `index` (0-based).
    fn page(&self, index: usize) -> Rc<str>;
    /// Pixel size of an embedded image, known without decoding it.
    fn image_size(&self, id: &str) -> Option<(u32, u32)>;
    /// The pixels of an embedded image, at most `width` pixels wide, or `None`
    /// if it can't be decoded.
    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage>;
    /// Whether an embedded image is a diagram, drawn in the terminal's colors.
    fn is_diagram(&self, _id: &str) -> bool {
        false
    }
    /// Whether an embedded image is a whole page drawn as designed, shown
    /// as tall as the column's width makes it rather than capped.
    fn is_page(&self, _id: &str) -> bool {
        false
    }
    /// A diagram drawn in `ink` on a transparent `paper`, at most `width` pixels wide.
    fn draw(&self, _id: &str, _width: u32, _ink: Rgb, _paper: Rgb) -> Option<RgbaImage> {
        None
    }
    /// How the document is shown, for one that can be shown two ways;
    /// `None` for one that can't.
    fn look(&self) -> Option<Look> {
        None
    }
    /// Shows the document another way. Pages keep their numbers, so the
    /// reader stays on the same page.
    fn set_look(&self, _look: Look) {}
}

/// The two ways an office document can be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// As its author designed it: its own fonts, colors and layout, drawn as a picture.
    Original,
    /// As text in the terminal's own font and colors.
    Terminal,
}

impl Look {
    pub fn other(self) -> Look {
        match self {
            Look::Original => Look::Terminal,
            Look::Terminal => Look::Original,
        }
    }
}

pub struct Doc {
    pub title: String,
    /// Total pages, when the source has pages; a source without pages is one page.
    pub page_count: Option<usize>,
    pub source: Box<dyn Source>,
}

impl Doc {
    /// A document whose markdown is known up front.
    pub fn whole(title: impl Into<String>, markdown: impl Into<String>) -> Self {
        Doc {
            title: title.into(),
            page_count: None,
            source: Box::new(Whole(markdown.into().into())),
        }
    }

    /// How many pages the viewer lays out.
    pub fn pages(&self) -> usize {
        self.page_count.unwrap_or(1)
    }

    /// The markdown href for an embedded image.
    pub fn image_href(id: &str) -> String {
        format!("{IMAGE_SCHEME}{id}")
    }

    pub fn image_ref(href: &str) -> ImageRef<'_> {
        match href.strip_prefix(IMAGE_SCHEME) {
            Some(id) => ImageRef::Embedded(id),
            None => ImageRef::Remote(href),
        }
    }
}

struct Whole(Rc<str>);

impl Source for Whole {
    fn page(&self, _index: usize) -> Rc<str> {
        self.0.clone()
    }
    fn image_size(&self, _id: &str) -> Option<(u32, u32)> {
        None
    }
    fn decode(&self, _id: &str, _width: u32) -> Option<RgbaImage> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_files_settle_and_failed_reads_retry() {
        let path = std::env::temp_dir().join(format!("leaf-watch-{}", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        let mut watch = Watch::new(path.clone());
        std::fs::write(&path, "new text").unwrap();
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|_| Err::<String, _>("busy")).is_none());
        watch.failed.as_mut().unwrap().1 -= RETRY;
        watch.checked -= WATCH_TICK;
        assert_eq!(
            watch.read(|path| std::fs::read_to_string(path)).as_deref(),
            Some("new text")
        );
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        std::fs::remove_file(&path).unwrap();
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        std::fs::write(&path, "recreated").unwrap();
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        watch.checked -= WATCH_TICK;
        assert_eq!(
            watch.read(|path| std::fs::read_to_string(path)).as_deref(),
            Some("recreated")
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_reloads_do_not_parse_every_tick() {
        let path = std::env::temp_dir().join(format!("leaf-watch-failure-{}", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        let mut watch = Watch::new(path.clone());
        std::fs::write(&path, "broken file").unwrap();
        let mut attempts = 0;
        for _ in 0..4 {
            watch.checked -= WATCH_TICK;
            watch.read(|_| {
                attempts += 1;
                Err::<(), _>(())
            });
        }
        assert_eq!(attempts, 1);
        std::fs::remove_file(path).unwrap();
    }

    fn finish_document(watch: &mut Watch) -> Option<Doc> {
        let until = Instant::now() + Duration::from_secs(5);
        let mut result = None;
        while watch.pending.is_some() && Instant::now() < until {
            result = watch.document(crate::load_file).or(result);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            watch.pending.is_none(),
            "the document loader did not finish"
        );
        result
    }

    #[test]
    fn paged_reloads_commit_only_successful_current_versions() {
        let path =
            std::env::temp_dir().join(format!("leaf-watch-paged-{}.epub", std::process::id()));
        let sample = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/sample.epub"));
        std::fs::copy(sample, &path).unwrap();
        let mut watch = Watch::new(path.clone());
        let original = watch.loaded;
        std::fs::write(&path, "broken epub").unwrap();
        for _ in 0..2 {
            watch.checked -= WATCH_TICK;
            assert!(watch.document(crate::load_file).is_none());
        }
        assert!(watch.pending.is_some());
        assert_eq!(watch.loaded, original);
        assert!(finish_document(&mut watch).is_none());
        assert_eq!(watch.loaded, original);
        assert!(watch.failed.is_some());
        watch.checked -= WATCH_TICK;
        assert!(watch.document(crate::load_file).is_none());
        assert!(
            watch.pending.is_none(),
            "failed versions wait before retrying"
        );

        std::fs::copy(sample, &path).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(42))
            .unwrap();
        for _ in 0..2 {
            watch.checked -= WATCH_TICK;
            assert!(watch.document(crate::load_file).is_none());
        }
        let doc = finish_document(&mut watch).expect("the replacement opens");
        assert_eq!(doc.title, "Test Book");
        assert!(doc.source.page(0).contains("Chapter One"));
        assert_eq!(watch.loaded, stamp(&path));
        assert!(watch.failed.is_none());
        drop(doc);

        let accepted = watch.loaded;
        file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(43))
            .unwrap();
        for _ in 0..2 {
            watch.checked -= WATCH_TICK;
            assert!(watch.document(crate::load_file).is_none());
        }
        assert!(watch.pending.is_some());
        let newer = path.with_extension("new.epub");
        std::fs::write(&newer, "newer version").unwrap();
        std::fs::rename(&newer, &path).unwrap();
        assert!(
            finish_document(&mut watch).is_none(),
            "obsolete results stay hidden"
        );
        assert_eq!(watch.loaded, accepted);
        assert!(
            watch.failed.is_none(),
            "obsolete failures do not delay newer files"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_changed_during_loading_is_not_acknowledged() {
        let path = std::env::temp_dir().join(format!("leaf-watch-race-{}", std::process::id()));
        std::fs::write(&path, "old").unwrap();
        let mut watch = Watch::new(path.clone());
        std::fs::write(&path, "first edit").unwrap();
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        watch.checked -= WATCH_TICK;
        assert!(
            watch
                .read(|path| {
                    std::fs::write(path, "another longer edit")?;
                    Ok::<_, std::io::Error>("first edit")
                })
                .is_none()
        );
        watch.checked -= WATCH_TICK;
        assert!(watch.read(|path| std::fs::read_to_string(path)).is_none());
        watch.checked -= WATCH_TICK;
        assert_eq!(
            watch.read(|path| std::fs::read_to_string(path)).as_deref(),
            Some("another longer edit")
        );
        std::fs::remove_file(path).unwrap();
    }
}
