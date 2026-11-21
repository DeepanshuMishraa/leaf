//! Measures a drive for `leaf storage`: walks the folders that matter on it
//! in parallel, counts every byte toward a category, and sends each thing
//! worth listing as soon as it's sized, so the screen fills in as it goes.
//! Sizes are blocks on disk, so sparse and cloud-only files count for what
//! they really take, and hard links count once.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::dirscan::{self, Kind};
use crate::junk::{self, Category, Finding, Role, Spot, Verdict};
use crate::volume::{self, Volume};

/// Files in a container this big are listed on their own.
const LISTED_FILE: u64 = 20_000_000;
/// Files anywhere else this big are listed on their own.
const HUGE_FILE: u64 = 500_000_000;
/// Share of a folder its biggest subfolder must hold to be named as where
/// most of it is.
const CULPRIT_SHARE: f64 = 0.4;
/// Below this depth folders are walked on the thread that found them.
const SPAWN_DEPTH: usize = 7;
/// Deepest the "mostly in" path goes.
const CULPRIT_DEPTH: usize = 8;

/// Where most of an item's space is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Culprit {
    /// Below the item, `/`-separated, ending in `/`.
    pub path: String,
    pub bytes: u64,
    /// Whether its name says it's a cache.
    pub cache: bool,
}

/// Something worth listing: a folder or file and what's known about it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Item {
    pub path: PathBuf,
    pub is_dir: bool,
    pub category: Category,
    pub finding: Option<Finding>,
    /// Everything in it: what deleting it frees.
    pub bytes: u64,
    /// What isn't also listed as something inside it.
    pub own: u64,
    pub files: u64,
    /// When anything in it last changed, in seconds since 1970; 0 unknown.
    pub newest: i64,
    pub culprit: Option<Culprit>,
}

impl Item {
    /// Whole days since anything in it changed.
    pub fn age_days(&self, now: i64) -> Option<u64> {
        (self.newest > 0).then(|| (now.saturating_sub(self.newest).max(0) / 86_400) as u64)
    }

    pub fn status(&self, now: i64) -> Option<junk::Status> {
        junk::status(self.finding.as_ref(), self.age_days(now))
    }
}

/// Counters the screen reads while a scan runs.
#[derive(Debug, Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
    /// Folders macOS refused to open (EPERM): behind Full Disk Access, or
    /// protected by System Integrity Protection whatever the access.
    pub protected: AtomicU64,
    /// Folders other accounts own (EACCES); only sudo reads them.
    pub private: AtomicU64,
    /// The first folders that couldn't be read, and why.
    pub unreadable: Mutex<Vec<Unreadable>>,
    categories: [AtomicU64; Category::ALL.len()],
    /// The folder being read, near the top of the tree.
    pub current: Mutex<PathBuf>,
    pub cancel: AtomicBool,
}

/// Most unreadable folders remembered by name.
const UNREADABLE_KEPT: usize = 200;

/// A folder that couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Unreadable {
    pub path: PathBuf,
    pub reason: Refusal,
}

/// Why a folder couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Refusal {
    /// macOS privacy or integrity protection (EPERM).
    Protected,
    /// Owned by another account (EACCES).
    Private,
}

impl Progress {
    fn refused(&self, path: &Path, reason: Refusal) {
        let counter = match reason {
            Refusal::Protected => &self.protected,
            Refusal::Private => &self.private,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut kept) = self.unreadable.lock()
            && kept.len() < UNREADABLE_KEPT
        {
            kept.push(Unreadable {
                path: path.to_owned(),
                reason,
            });
        }
    }

    pub fn category(&self, category: Category) -> u64 {
        self.categories[category.index()].load(Ordering::Relaxed)
    }

    fn add(&self, category: Category, bytes: u64) {
        self.categories[category.index()].fetch_add(bytes, Ordering::Relaxed);
    }

    /// Takes `bytes` off a category, after something in it was deleted.
    pub fn remove(&self, category: Category, bytes: u64) {
        let counter = &self.categories[category.index()];
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
            Some(now.saturating_sub(bytes))
        });
    }
}

/// What a running scan sends.
#[derive(Debug)]
pub enum Event {
    Found(Item),
    /// Every root has been walked.
    Done,
}

/// A scan in progress.
pub struct Scan {
    pub progress: Arc<Progress>,
    pub events: Receiver<Event>,
    pub roots: Vec<PathBuf>,
}

impl Scan {
    pub fn cancel(&self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }
}

/// Starts measuring `volume` on a background thread.
pub fn start(volume: &Volume, all: &[Volume]) -> Scan {
    let roots = roots(volume, all);
    start_roots(roots)
}

/// Starts measuring just `roots`, each as its own drive would be.
pub fn start_roots(roots: Vec<PathBuf>) -> Scan {
    let progress = Arc::new(Progress::default());
    let (send, events) = mpsc::channel();
    let shared = progress.clone();
    let walked = roots.clone();
    std::thread::spawn(move || {
        let walker = Walker::new(&shared, send.clone());
        let temps: Vec<&Path> = walker.temps.iter().map(PathBuf::as_path).collect();
        let home = walker.home.as_deref();
        for root in &walked {
            let role = junk::root_role(root, home, &temps);
            walker.root(root, role);
        }
        let _ = send.send(Event::Done);
    });
    Scan {
        progress,
        events,
        roots,
    }
}

/// The folders that matter on `volume`: on the startup disk the home
/// folder, apps and temporary folders; on any other drive, all of it.
pub fn roots(volume: &Volume, all: &[Volume]) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.extend(home());
    if cfg!(target_os = "macos") {
        candidates.extend(
            [
                "/Applications",
                "/Library",
                "/opt/homebrew",
                "/usr/local",
                "/private/tmp",
            ]
            .map(PathBuf::from),
        );
    } else {
        candidates.extend(
            [
                "/opt",
                "/usr/local",
                "/tmp",
                "/var/tmp",
                "/var/cache",
                "/var/log",
                "/snap",
            ]
            .map(PathBuf::from),
        );
    }
    candidates.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
    let mine = all.iter().position(|v| v.mount == volume.mount);
    let mut roots: Vec<PathBuf> = Vec::new();
    for candidate in candidates {
        let Ok(path) = std::fs::canonicalize(&candidate) else {
            continue;
        };
        let owned = volume::containing(all, &path) == mine;
        let covered = roots
            .iter()
            .any(|r| path.starts_with(r) || r.starts_with(&path));
        if owned && !covered {
            roots.push(path);
        }
    }
    if roots.is_empty() {
        roots.push(volume.mount.clone());
    }
    roots
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .and_then(|h| std::fs::canonicalize(h).ok())
}

/// The temporary folders, where agents leave scratch files.
fn temps() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["/tmp", "/private/tmp", "/var/tmp"]
        .iter()
        .map(PathBuf::from)
        .chain(std::env::var_os("TMPDIR").map(PathBuf::from))
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    out.dedup();
    out
}

/// How a folder is walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Looking for items, and with `list`, listing each folder in it.
    Free { list: bool },
    /// Each thing in it is an item.
    Container(Option<Finding>),
    /// Inside an item: only sized.
    Item,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct State {
    category: Category,
    mode: Mode,
}

impl State {
    fn of(role: Role) -> State {
        match role {
            Role::Item { category, .. } => State {
                category,
                mode: Mode::Item,
            },
            Role::Container { category, finding } => State {
                category,
                mode: Mode::Container(finding),
            },
            Role::Area { category, list } => State {
                category,
                mode: Mode::Free { list },
            },
        }
    }
}

/// Whether a container holds your own folders, which are searched for
/// build output and dependencies inside.
fn holds_your_files(category: Category) -> bool {
    matches!(
        category,
        Category::Documents | Category::Media | Category::Other
    )
}

/// What becomes of a folder once it's walked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listing {
    /// Nothing: it's more of what it's in.
    No,
    /// An item, with what's known of it.
    Item(Option<Finding>),
    /// A folder listed in an area or someone's container, with what's known
    /// of it; items inside it are listed too.
    Folder(Option<Finding>),
}

/// What a walked folder adds up to.
#[derive(Debug, Default)]
struct Sized {
    bytes: u64,
    /// Not counting items inside it.
    own: u64,
    files: u64,
    newest: i64,
    /// Where most of it is, biggest first.
    chain: Vec<(String, u64)>,
}

struct Walker<'a> {
    progress: &'a Progress,
    send: Sender<Event>,
    home: Option<PathBuf>,
    temps: Vec<PathBuf>,
    /// Threads that may still be started.
    slots: AtomicUsize,
    /// Files with more than one link already counted.
    links: Mutex<HashSet<(u64, u64)>>,
}

impl<'a> Walker<'a> {
    fn new(progress: &'a Progress, send: Sender<Event>) -> Self {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        Walker {
            progress,
            send,
            home: home(),
            temps: temps(),
            slots: AtomicUsize::new((threads * 4).clamp(8, 48)),
            links: Mutex::new(HashSet::new()),
        }
    }

    fn root(&self, root: &Path, role: Role) {
        let Ok(meta) = std::fs::symlink_metadata(root) else {
            return;
        };
        let state = State::of(role);
        let sized = self.walk(root, meta.dev(), state, 0);
        // A root that's itself one thing, like a lone trash folder.
        if let Role::Item { category, finding } = role {
            self.emit(root, true, category, finding, &sized, sized.bytes);
        }
    }

    fn take_slot(&self) -> bool {
        self.slots
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    }

    fn give_slot(&self) {
        self.slots.fetch_add(1, Ordering::AcqRel);
    }

    fn rel<'p>(&self, path: &'p Path) -> Option<&'p str> {
        path.strip_prefix(self.home.as_deref()?).ok()?.to_str()
    }

    /// What a folder in a folder walked in `parent` is, and how it's walked.
    fn plan(&self, path: &Path, name: &str, parent: State) -> (Listing, State) {
        if parent.mode == Mode::Item {
            return (Listing::No, parent);
        }
        let in_temp = matches!(parent.mode, Mode::Container(Some(f)) if f.verdict == Verdict::Temp);
        let spot = Spot {
            path,
            name,
            rel: self.rel(path),
            temp: in_temp,
        };
        match junk::classify(&spot) {
            Some(role @ Role::Item { finding, .. }) => (Listing::Item(finding), State::of(role)),
            Some(role) => (Listing::No, State::of(role)),
            None => match parent.mode {
                // Junk is cleared whole, so nothing inside needs finding.
                Mode::Container(
                    finding @ Some(Finding {
                        verdict: Verdict::Junk | Verdict::Temp,
                        ..
                    }),
                ) => (
                    Listing::Item(finding),
                    State {
                        mode: Mode::Item,
                        ..parent
                    },
                ),
                // Apps and their data are left whole: a node_modules in an
                // app bundle is part of the app.
                Mode::Container(finding) if !holds_your_files(parent.category) => (
                    Listing::Item(finding),
                    State {
                        mode: Mode::Item,
                        ..parent
                    },
                ),
                // Your folders: listed, and searched for junk inside.
                Mode::Container(finding) => (
                    Listing::Folder(finding),
                    State {
                        mode: Mode::Free { list: false },
                        ..parent
                    },
                ),
                Mode::Free { list: true } => (
                    Listing::Folder(None),
                    State {
                        mode: Mode::Free { list: false },
                        ..parent
                    },
                ),
                Mode::Free { list: false } | Mode::Item => (Listing::No, parent),
            },
        }
    }

    fn walk(&self, dir: &Path, dev: u64, state: State, depth: usize) -> Sized {
        if self.progress.cancel.load(Ordering::Relaxed) {
            return Sized::default();
        }
        if depth <= 4
            && let Ok(mut current) = self.progress.current.lock()
        {
            current.clear();
            current.push(dir);
        }
        let entries = match dirscan::read(dir) {
            Ok(entries) => entries,
            Err(e) => {
                match e.raw_os_error() {
                    Some(libc::EPERM) => self.progress.refused(dir, Refusal::Protected),
                    Some(libc::EACCES) => self.progress.refused(dir, Refusal::Private),
                    _ => {}
                }
                return Sized::default();
            }
        };
        let mut sized = Sized::default();
        let mut subdirs = Vec::new();
        let mut counted = 0u64;
        for entry in entries {
            match entry.kind {
                Kind::Other => continue,
                Kind::Dir => {
                    if entry.dev == dev {
                        subdirs.push(dir.join(&entry.name));
                    }
                    continue;
                }
                Kind::File => {}
            }
            if entry.nlink > 1
                && let Ok(mut links) = self.links.lock()
                && !links.insert((entry.dev, entry.ino))
            {
                continue;
            }
            let bytes = entry.bytes;
            sized.files += 1;
            sized.bytes += bytes;
            sized.newest = sized.newest.max(entry.mtime);
            counted += bytes;
            let listed = match state.mode {
                Mode::Container(_) => bytes >= LISTED_FILE,
                Mode::Free { .. } => bytes >= HUGE_FILE,
                Mode::Item => false,
            };
            if listed {
                let finding = match state.mode {
                    Mode::Container(finding) => finding,
                    _ => None,
                };
                let file = Sized {
                    bytes,
                    own: bytes,
                    files: 1,
                    newest: entry.mtime,
                    chain: Vec::new(),
                };
                self.emit(
                    &dir.join(&entry.name),
                    false,
                    state.category,
                    finding,
                    &file,
                    bytes,
                );
            } else {
                sized.own += bytes;
            }
        }
        self.progress.add(state.category, counted);
        self.progress
            .files
            .fetch_add(sized.files, Ordering::Relaxed);
        self.progress.bytes.fetch_add(counted, Ordering::Relaxed);

        let children = self.walk_children(subdirs, dev, state, depth);
        let mut biggest: Option<(String, Sized)> = None;
        for (path, listing, child_state, child) in children {
            sized.bytes += child.bytes;
            sized.files += child.files;
            sized.newest = sized.newest.max(child.newest);
            match listing {
                Listing::No => sized.own += child.own,
                Listing::Folder(finding) => {
                    sized.own += child.own;
                    self.emit(
                        &path,
                        true,
                        child_state.category,
                        finding,
                        &child,
                        child.own,
                    );
                }
                Listing::Item(finding) => {
                    self.emit(
                        &path,
                        true,
                        child_state.category,
                        finding,
                        &child,
                        child.bytes,
                    );
                }
            }
            if biggest.as_ref().is_none_or(|(_, b)| child.bytes > b.bytes) {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                biggest = Some((name, child));
            }
        }
        if let Some((name, child)) = biggest
            && child.bytes > 0
            && child.bytes as f64 >= sized.bytes as f64 * CULPRIT_SHARE
        {
            sized.chain.push((name, child.bytes));
            sized
                .chain
                .extend(child.chain.into_iter().take(CULPRIT_DEPTH - 1));
        }
        sized
    }

    /// Walks the folders in a folder, on other threads while any are free.
    fn walk_children(
        &self,
        subdirs: Vec<PathBuf>,
        dev: u64,
        state: State,
        depth: usize,
    ) -> Vec<(PathBuf, Listing, State, Sized)> {
        if subdirs.is_empty() {
            return Vec::new();
        }
        std::thread::scope(|scope| {
            let mut running = Vec::new();
            let mut done = Vec::with_capacity(subdirs.len());
            for path in subdirs {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (listing, child_state) = self.plan(&path, &name, state);
                // Deep down, folders are small: a thread costs more than it saves.
                if depth < SPAWN_DEPTH && self.take_slot() {
                    let walked = path.clone();
                    let handle = scope.spawn(move || {
                        let sized = self.walk(&walked, dev, child_state, depth + 1);
                        self.give_slot();
                        sized
                    });
                    running.push((path, listing, child_state, handle));
                } else {
                    let sized = self.walk(&path, dev, child_state, depth + 1);
                    done.push((path, listing, child_state, sized));
                }
            }
            for (path, listing, child_state, handle) in running {
                done.push((
                    path,
                    listing,
                    child_state,
                    handle.join().unwrap_or_default(),
                ));
            }
            done
        })
    }

    fn emit(
        &self,
        path: &Path,
        is_dir: bool,
        category: Category,
        finding: Option<Finding>,
        sized: &Sized,
        own: u64,
    ) {
        if sized.bytes == 0 {
            return;
        }
        let culprit = sized.chain.last().map(|&(_, bytes)| Culprit {
            path: sized
                .chain
                .iter()
                .map(|(name, _)| format!("{name}/"))
                .collect(),
            bytes,
            cache: sized
                .chain
                .iter()
                .any(|(name, _)| junk::sounds_like_cache(name)),
        });
        let _ = self.send.send(Event::Found(Item {
            path: path.to_owned(),
            is_dir,
            category,
            finding,
            bytes: sized.bytes,
            own,
            files: sized.files,
            newest: sized.newest,
            culprit,
        }));
    }
}

/// One thing in a folder being browsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub is_dir: bool,
    pub bytes: u64,
    pub files: u64,
    pub newest: i64,
    pub finding: Option<Finding>,
}

/// Measures each thing in `dir` on a background thread, sending each one
/// as it's sized, then `None`.
pub fn browse(dir: &Path) -> (Arc<Progress>, Receiver<Option<Entry>>) {
    let progress = Arc::new(Progress::default());
    let (send, receive) = mpsc::channel();
    let shared = progress.clone();
    let dir = dir.to_owned();
    std::thread::spawn(move || {
        let (events, _) = mpsc::channel();
        let walker = Walker::new(&shared, events);
        let inside = State {
            category: Category::Other,
            mode: Mode::Item,
        };
        let Ok(entries) = dirscan::read(&dir) else {
            let _ = send.send(None);
            return;
        };
        let dev = std::fs::metadata(&dir).map(|m| m.dev()).unwrap_or(0);
        let mut subdirs = Vec::new();
        for entry in entries {
            if entry.kind == Kind::Other {
                continue;
            }
            let path = dir.join(&entry.name);
            let finding = finding_for(&walker, &path);
            if entry.kind == Kind::Dir {
                if entry.dev == dev {
                    subdirs.push((path, finding));
                }
                continue;
            }
            let _ = send.send(Some(Entry {
                path,
                is_dir: false,
                bytes: entry.bytes,
                files: 1,
                newest: entry.mtime,
                finding,
            }));
        }
        std::thread::scope(|scope| {
            for (path, finding) in subdirs {
                let (walker, send) = (&walker, send.clone());
                let run = move || {
                    let sized = walker.walk(&path, dev, inside, 1);
                    let _ = send.send(Some(Entry {
                        path,
                        is_dir: true,
                        bytes: sized.bytes,
                        files: sized.files,
                        newest: sized.newest,
                        finding,
                    }));
                };
                if walker.take_slot() {
                    scope.spawn(move || {
                        run();
                        walker.give_slot();
                    });
                } else {
                    run();
                }
            }
        });
        let _ = send.send(None);
    });
    (progress, receive)
}

/// What's known of `path` on its own, for browsing.
fn finding_for(walker: &Walker, path: &Path) -> Option<Finding> {
    let name = path.file_name()?.to_str()?;
    let spot = Spot {
        path,
        name,
        rel: walker.rel(path),
        temp: path
            .parent()
            .is_some_and(|p| walker.temps.iter().any(|t| t == p)),
    };
    match junk::classify(&spot)? {
        Role::Item { finding, .. } | Role::Container { finding, .. } => finding,
        Role::Area { .. } => None,
    }
}

/// Why something couldn't be deleted.
#[derive(Debug, thiserror::Error)]
#[error("couldn't delete {}: {source}", path.display())]
pub struct DeleteFailed {
    pub path: PathBuf,
    #[source]
    pub source: io::Error,
}

/// Deletes `path` for good: a folder with everything in it, or a file.
pub fn delete(path: &Path) -> Result<(), DeleteFailed> {
    let meta = std::fs::symlink_metadata(path).map_err(|source| DeleteFailed {
        path: path.to_owned(),
        source,
    })?;
    let result = if meta.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|source| DeleteFailed {
        path: path.to_owned(),
        source,
    })
}

/// Whether this process (that is, the terminal it runs in) has Full Disk
/// Access: it can list folders only that access opens, such as Safari's.
/// `None` off macOS, or when none of them exist to tell by.
pub fn full_disk_access() -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let home = home()?;
    ["Library/Safari", "Library/Mail", "Library/Messages"]
        .iter()
        .find_map(|guarded| match std::fs::read_dir(home.join(guarded)) {
            Ok(_) => Some(true),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => Some(false),
            Err(_) => None,
        })
}

/// Seconds since 1970, now.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Scans `root` as a drive of its own, to the end, for tests and `--json`.
pub fn scan_to_end(scan: &Scan) -> Vec<Item> {
    let mut items = Vec::new();
    for event in scan.events.iter() {
        match event {
            Event::Found(item) => items.push(item),
            Event::Done => break,
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("leaf-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let write = |rel: &str, bytes: usize| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![7u8; bytes]).unwrap();
        };
        write("app/node_modules/react/index.js", 200_000);
        write("app/node_modules/react/big/blob.bin", 900_000);
        write("app/src/main.js", 10_000);
        write("app/package.json", 100);
        write("crate/target/CACHEDIR.TAG", 100);
        write("crate/target/debug/out.bin", 300_000);
        write("notes/today.md", 5_000);
        root
    }

    #[test]
    fn a_drive_lists_its_folders_and_finds_build_output_inside_them() {
        let root = tree("scan");
        let scan = start_roots(vec![root.clone()]);
        let items = scan_to_end(&scan);
        let find = |end: &str| items.iter().find(|i| i.path.ends_with(end));

        let modules = find("app/node_modules").expect("node_modules is found");
        assert_eq!(modules.category, Category::Developer);
        assert_eq!(modules.finding.map(|f| f.verdict), Some(Verdict::Junk));
        assert!(modules.bytes >= 1_100_000);
        let culprit = modules.culprit.as_ref().expect("most of it is in react/");
        assert!(culprit.path.starts_with("react/"), "{culprit:?}");

        assert!(find("crate/target").is_some(), "cargo output by its tag");
        let app = find("app").expect("top folders are listed");
        assert!(app.finding.is_none());
        assert!(
            app.own < app.bytes,
            "node_modules is counted under Developer"
        );
        assert!(scan.progress.category(Category::Developer) >= modules.bytes);
        assert_eq!(
            scan.progress.category(Category::Other) + scan.progress.category(Category::Developer),
            scan.progress.bytes.load(Ordering::Relaxed)
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn browsing_sizes_each_thing_in_a_folder() {
        let root = tree("browse");
        let (_, entries) = browse(&root.join("app"));
        let mut listed: Vec<Entry> = entries.iter().map_while(|e| e).collect();
        listed.sort_by_key(|e| std::cmp::Reverse(e.bytes));
        assert_eq!(listed.len(), 3, "node_modules, src and package.json");
        assert!(listed[0].path.ends_with("node_modules"));
        assert_eq!(listed[0].finding.map(|f| f.label), Some("node_modules"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleting_removes_folders_and_reports_what_failed() {
        let root = tree("delete");
        delete(&root.join("app/node_modules")).unwrap();
        assert!(!root.join("app/node_modules").exists());
        let missing = delete(&root.join("nothing-here")).unwrap_err();
        assert!(missing.to_string().contains("nothing-here"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_folders_are_counted_by_why() {
        let progress = Progress::default();
        progress.refused(Path::new("/a"), Refusal::Protected);
        progress.refused(Path::new("/b"), Refusal::Private);
        progress.refused(Path::new("/c"), Refusal::Private);
        assert_eq!(progress.protected.load(Ordering::Relaxed), 1);
        assert_eq!(progress.private.load(Ordering::Relaxed), 2);
        let kept = progress.unreadable.lock().unwrap();
        assert_eq!(kept[0].path, PathBuf::from("/a"));
        assert_eq!(kept[0].reason, Refusal::Protected);
    }
}
