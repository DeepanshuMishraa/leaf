//! Diffs: a patch parsed into rows that can be drawn without laying out the
//! whole thing, plus the Git commands that produce patches.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use regex::Regex;

use crate::comment::{Anchor, Side};
use crate::github;

/// A patch larger than this is refused rather than held in memory.
const MAX_PATCH_BYTES: usize = 96 * 1024 * 1024;
/// An untracked file larger than this is named but not shown.
const MAX_UNTRACKED_BYTES: u64 = 2 * 1024 * 1024;
/// Enough to tell text from binary.
const SNIFF_BYTES: usize = 8192;

static HUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$")
        .expect("the hunk header pattern is valid")
});

/// What a row of a diff is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The `diff --git` line, drawn as a file heading.
    File,
    /// An `@@ … @@` line.
    Hunk,
    /// A note such as a binary file or a mode change.
    Meta,
    /// A line present on both sides.
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`.
    NoNewline,
}

impl Kind {
    pub fn is_content(self) -> bool {
        matches!(self, Kind::Context | Kind::Added | Kind::Removed)
    }
}

/// One drawn row. Text lives in `Diff::patch` at `start..start + len`, so a
/// million-row diff costs 24 bytes a row on top of the patch itself.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub start: u32,
    pub len: u32,
    pub old: u32,
    pub new: u32,
    pub file: u32,
    /// Index into the file's hunks; `u32::MAX` outside a hunk.
    pub hunk: u32,
    pub kind: Kind,
}

/// A hunk and the rows it covers. `first..end` is the content, exclusive of
/// the header row.
#[derive(Debug, Clone)]
pub struct Hunk {
    pub first: usize,
    pub end: usize,
}

/// A file in a patch. `old_path` is `None` for a new file and `new_path` is
/// `None` for a deleted one.
#[derive(Debug, Clone)]
pub struct File {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub header_row: usize,
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    /// A short note for the heading, such as `new file` or `mode changed`.
    pub note: Option<String>,
}

impl File {
    /// The path to show: the new one when there is one.
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or("")
    }

    pub fn display(&self) -> String {
        match (&self.old_path, &self.new_path) {
            (Some(old), Some(new)) if old != new => format!("{old} → {new}"),
            _ => self.path().to_owned(),
        }
    }
}

/// A patch, split into rows. Parsing is one pass and allocates only the patch,
/// one `Row` per line, and the path strings.
pub struct Diff {
    patch: String,
    rows: Vec<Row>,
    files: Vec<File>,
    max_line: u32,
}

impl Diff {
    pub fn parse(patch: String) -> Self {
        let (rows, files, max_line) = Builder::new(&patch).parse();
        Diff {
            patch,
            rows,
            files,
            max_line: max_line.max(1),
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn files(&self) -> &[File] {
        &self.files
    }

    pub fn max_line(&self) -> u32 {
        self.max_line
    }

    pub fn file(&self, index: usize) -> Option<&File> {
        self.files.get(index)
    }

    pub fn file_index(&self, row: usize) -> Option<usize> {
        self.rows
            .get(row)
            .map(|row| row.file as usize)
            .filter(|&file| file < self.files.len())
    }

    /// The path a comment on `side` hangs off.
    pub fn path(&self, file: usize, side: Side) -> &str {
        let Some(file) = self.files.get(file) else {
            return "";
        };
        match side {
            Side::Old => file.old_path.as_deref().or(file.new_path.as_deref()),
            Side::New => file.new_path.as_deref().or(file.old_path.as_deref()),
        }
        .unwrap_or("")
    }

    /// The row's text without its line ending.
    pub fn row_text(&self, row: usize) -> &str {
        let Some(row) = self.rows.get(row) else {
            return "";
        };
        let text = self
            .patch
            .get(row.start as usize..(row.start + row.len) as usize)
            .unwrap_or("");
        strip_eol(text)
    }

    /// A content row's text without its `+`, `-` or space marker.
    pub fn content(&self, row: usize) -> &str {
        let text = self.row_text(row);
        text.get(1..).unwrap_or("")
    }

    pub fn first_content(&self, file: usize) -> Option<usize> {
        let file = self.files.get(file)?;
        file.hunks
            .first()
            .map(|hunk| hunk.first)
            .or(Some(file.header_row))
    }

    /// The next row that holds a code line, skipping headings and notes.
    pub fn next_content(&self, row: usize, forward: bool) -> Option<usize> {
        if forward {
            (row + 1..self.rows.len()).find(|&i| self.rows[i].kind.is_content())
        } else {
            (0..row.min(self.rows.len()))
                .rev()
                .find(|&i| self.rows[i].kind.is_content())
        }
    }

    pub fn first_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.kind.is_content())
            .or((!self.rows.is_empty()).then_some(0))
    }

    pub fn last_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .rposition(|row| row.kind.is_content())
            .or_else(|| self.rows.len().checked_sub(1))
    }

    /// The row for a line on one side of a file, when the diff still has it.
    pub fn row_for(&self, file: &str, side: Side, line: u32) -> Option<usize> {
        let wanted = self.files.iter().position(|f| match side {
            Side::Old => f.old_path.as_deref() == Some(file),
            Side::New => f.new_path.as_deref() == Some(file),
        });
        if let Some(index) = wanted {
            let f = &self.files[index];
            for hunk in &f.hunks {
                for row in hunk.first..hunk.end {
                    let r = self.rows[row];
                    let number = match side {
                        Side::Old => r.old,
                        Side::New => r.new,
                    };
                    if number == line {
                        return Some(row);
                    }
                }
            }
            return self.first_content(index);
        }
        // A path may have come from another revision; land on a file with the
        // same tail rather than nowhere.
        self.files
            .iter()
            .position(|f| {
                f.path()
                    .rsplit('/')
                    .next()
                    .zip(file.rsplit('/').next())
                    .is_some_and(|(a, b)| a == b)
            })
            .and_then(|index| self.first_content(index))
    }

    /// The rows a comment on `start..=end` covers, clamped to one hunk, and
    /// the anchor GitHub would use.
    pub fn comment_range(&self, start: usize, end: usize) -> Option<(usize, usize, Anchor)> {
        let (start, end) = (start.min(end), start.max(end));
        let head = (start..=end)
            .rev()
            .find(|&row| self.rows.get(row).is_some_and(|r| r.kind.is_content()))?;
        let file_index = self.rows[head].file as usize;
        let hunk_index = self.rows[head].hunk as usize;
        let hunk = self.files.get(file_index)?.hunks.get(hunk_index)?;

        let low = start.max(hunk.first);
        let high = end.min(hunk.end.saturating_sub(1));
        if low > high {
            return None;
        }
        let first = (low..=high).find(|&row| self.rows[row].kind.is_content())?;
        let last = (low..=high)
            .rev()
            .find(|&row| self.rows[row].kind.is_content())?;

        let rows = &self.rows[first..=last];
        let side = if rows.iter().all(|row| row.kind == Kind::Removed) {
            Side::Old
        } else {
            Side::New
        };
        let numbers: Vec<u32> = rows
            .iter()
            .filter_map(|row| match side {
                Side::Old => (row.old > 0).then_some(row.old),
                Side::New => (row.new > 0).then_some(row.new),
            })
            .collect();
        let (&start_line, &end_line) = (numbers.first()?, numbers.last()?);

        Some((
            first,
            last,
            Anchor {
                file: self.path(file_index, side).to_owned(),
                side,
                start_line,
                end_line,
            },
        ))
    }

    /// The patch lines `first..=last`, without the final line ending.
    pub fn snippet(&self, first: usize, last: usize) -> String {
        let (first, last) = (first.min(last), first.max(last));
        let (Some(first), Some(last)) = (self.rows.get(first), self.rows.get(last)) else {
            return String::new();
        };
        let start = first.start as usize;
        let end = (last.start + last.len) as usize;
        self.patch
            .get(start..end)
            .unwrap_or("")
            .trim_end_matches(['\n', '\r'])
            .to_owned()
    }
}

/// Turns lines into rows in one pass, tracking the file and hunk each line
/// belongs to.
struct Builder<'a> {
    patch: &'a str,
    rows: Vec<Row>,
    files: Vec<File>,
    file: Option<usize>,
    hunk: Option<usize>,
    last_hunk: Option<usize>,
    old_left: u32,
    new_left: u32,
    old_line: u32,
    new_line: u32,
    max_line: u32,
}

impl<'a> Builder<'a> {
    fn new(patch: &'a str) -> Self {
        Builder {
            patch,
            rows: Vec::new(),
            files: Vec::new(),
            file: None,
            hunk: None,
            last_hunk: None,
            old_left: 0,
            new_left: 0,
            old_line: 0,
            new_line: 0,
            max_line: 0,
        }
    }

    fn parse(mut self) -> (Vec<Row>, Vec<File>, u32) {
        let patch = self.patch;
        let mut offset = 0;
        for raw in patch.split_inclusive('\n') {
            let start = offset;
            let len = raw.len();
            offset += len;
            self.line(strip_eol(raw), start, len);
        }
        (self.rows, self.files, self.max_line)
    }

    fn line(&mut self, text: &str, start: usize, len: usize) {
        if text.is_empty() {
            return;
        }
        if text.starts_with('\\') {
            self.no_newline(start, len);
            return;
        }
        if self.old_left > 0 || self.new_left > 0 {
            if self.content(text, start, len) {
                return;
            }
            self.hunk = None;
        }

        if let Some(rest) = text.strip_prefix("diff --git ") {
            let (old, new) = git_header_paths(rest);
            self.begin_file(start, len, old, new);
        } else if text.starts_with("@@ ") {
            self.begin_hunk(text, start, len);
        } else if let Some(rest) = text.strip_prefix("--- ") {
            let path = marker_path(rest);
            match self.file {
                Some(file) => self.files[file].old_path = path,
                None => self.begin_file(start, len, path, None),
            }
        } else if let Some(rest) = text.strip_prefix("+++ ") {
            let path = marker_path(rest);
            match self.file {
                Some(file) => self.files[file].new_path = path,
                None => self.begin_file(start, len, None, path),
            }
        } else if let Some(rest) = text
            .strip_prefix("rename from ")
            .or_else(|| text.strip_prefix("copy from "))
        {
            self.set_old_path(strip_ab(rest));
        } else if let Some(rest) = text
            .strip_prefix("rename to ")
            .or_else(|| text.strip_prefix("copy to "))
        {
            self.set_new_path(strip_ab(rest));
        } else if text.starts_with("new file mode ") {
            self.note("new file");
        } else if text.starts_with("deleted file mode ") {
            self.note("deleted");
        } else if text.starts_with("old mode ") || text.starts_with("new mode ") {
            self.note("mode changed");
        } else if text.starts_with("Binary files ") || text.starts_with("GIT binary patch") {
            if let Some(file) = self.file {
                self.files[file].binary = true;
                self.files[file].note = Some("binary file, not shown".to_owned());
            }
            self.meta(start, len);
        } else if text.starts_with("leaf: ") {
            self.meta(start, len);
        } else if text.starts_with("index ")
            || text.starts_with("similarity index ")
            || text.starts_with("dissimilarity index ")
        {
            // Plumbing that adds nothing for a reader.
        }
    }

    fn begin_file(&mut self, start: usize, len: usize, old: Option<String>, new: Option<String>) {
        let index = self.files.len();
        self.files.push(File {
            old_path: old,
            new_path: new,
            header_row: self.rows.len(),
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            binary: false,
            note: None,
        });
        self.rows.push(Row {
            start: start as u32,
            len: len as u32,
            old: 0,
            new: 0,
            file: index as u32,
            hunk: u32::MAX,
            kind: Kind::File,
        });
        self.file = Some(index);
        self.hunk = None;
        self.last_hunk = None;
        self.old_left = 0;
        self.new_left = 0;
        self.old_line = 0;
        self.new_line = 0;
    }

    fn begin_hunk(&mut self, text: &str, start: usize, len: usize) {
        let Some(file) = self.file else {
            return;
        };
        let Some(caps) = HUNK.captures(text) else {
            return;
        };
        let number = |index: usize, default: u32| {
            caps.get(index)
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(default)
        };
        let old_start = number(1, 1);
        let old_count = number(2, 1);
        let new_start = number(3, 1);
        let new_count = number(4, 1);

        let hunk = self.files[file].hunks.len();
        let header_row = self.rows.len();
        self.rows.push(Row {
            start: start as u32,
            len: len as u32,
            old: 0,
            new: 0,
            file: file as u32,
            hunk: hunk as u32,
            kind: Kind::Hunk,
        });
        self.files[file].hunks.push(Hunk {
            first: header_row + 1,
            end: header_row + 1,
        });
        self.hunk = Some(hunk);
        self.last_hunk = Some(hunk);
        self.old_left = old_count;
        self.new_left = new_count;
        self.old_line = old_start;
        self.new_line = new_start;
    }

    /// A `+`, `-` or context line; false when `text` isn't one.
    fn content(&mut self, text: &str, start: usize, len: usize) -> bool {
        let Some(marker) = text.as_bytes().first().copied() else {
            return false;
        };
        if !matches!(marker, b' ' | b'+' | b'-') {
            return false;
        }
        let (Some(file), Some(hunk)) = (self.file, self.hunk) else {
            return false;
        };
        let old = if marker == b'+' { 0 } else { self.old_line };
        let new = if marker == b'-' { 0 } else { self.new_line };
        let kind = match marker {
            b'+' => Kind::Added,
            b'-' => Kind::Removed,
            _ => Kind::Context,
        };
        self.rows.push(Row {
            start: start as u32,
            len: len as u32,
            old,
            new,
            file: file as u32,
            hunk: hunk as u32,
            kind,
        });
        if marker != b'+' {
            self.old_line += 1;
            self.old_left = self.old_left.saturating_sub(1);
        }
        if marker != b'-' {
            self.new_line += 1;
            self.new_left = self.new_left.saturating_sub(1);
        }
        self.max_line = self.max_line.max(old).max(new);
        self.files[file].hunks[hunk].end = self.rows.len();
        match kind {
            Kind::Added => self.files[file].added += 1,
            Kind::Removed => self.files[file].removed += 1,
            _ => {}
        }
        if self.old_left == 0 && self.new_left == 0 {
            self.hunk = None;
        }
        true
    }

    fn no_newline(&mut self, start: usize, len: usize) {
        let (Some(file), Some(hunk)) = (self.file, self.last_hunk) else {
            return;
        };
        self.rows.push(Row {
            start: start as u32,
            len: len as u32,
            old: 0,
            new: 0,
            file: file as u32,
            hunk: hunk as u32,
            kind: Kind::NoNewline,
        });
    }

    fn meta(&mut self, start: usize, len: usize) {
        let Some(file) = self.file else {
            return;
        };
        self.rows.push(Row {
            start: start as u32,
            len: len as u32,
            old: 0,
            new: 0,
            file: file as u32,
            hunk: self.last_hunk.map_or(u32::MAX, |hunk| hunk as u32),
            kind: Kind::Meta,
        });
    }

    fn note(&mut self, text: &str) {
        if let Some(file) = self.file {
            self.files[file].note = Some(text.to_owned());
        }
    }

    fn set_old_path(&mut self, path: String) {
        if let Some(file) = self.file {
            self.files[file].old_path = Some(path);
        }
    }

    fn set_new_path(&mut self, path: String) {
        if let Some(file) = self.file {
            self.files[file].new_path = Some(path);
        }
    }
}

fn strip_eol(text: &str) -> &str {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.strip_suffix('\r').unwrap_or(text)
}

/// `a/old b/new`, quoted when a path needs it.
fn git_header_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let Some((old, used)) = unquote(rest) else {
            return (None, None);
        };
        let tail = rest[used..].trim_start();
        let new = if tail.starts_with('"') {
            unquote(tail).map(|(path, _)| path)
        } else {
            Some(tail.to_owned())
        };
        return (clean_path(&old), new.and_then(|path| clean_path(&path)));
    }
    if let Some(at) = rest.find(" b/") {
        return (clean_path(&rest[..at]), clean_path(&rest[at + 1..]));
    }
    match rest.split_once(' ') {
        Some((old, new)) => (clean_path(old), clean_path(new)),
        None => (clean_path(rest), None),
    }
}

/// The path after `--- ` or `+++ `, dropping a timestamp and `/dev/null`.
fn marker_path(rest: &str) -> Option<String> {
    let rest = if rest.starts_with('"') {
        unquote(rest).map(|(path, _)| path)?
    } else {
        rest.split('\t').next().unwrap_or(rest).to_owned()
    };
    clean_path(&rest)
}

fn clean_path(path: &str) -> Option<String> {
    let path = path.trim_end();
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    Some(strip_ab(path))
}

fn strip_ab(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_owned()
}

/// Reads a Git C-quoted string, returning the text and the bytes it used.
fn unquote(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), i + 1)),
            b'\\' => {
                i += 1;
                let byte = *bytes.get(i)?;
                match byte {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'a' => out.push(0x07),
                    b'b' => out.push(0x08),
                    b'f' => out.push(0x0c),
                    b'v' => out.push(0x0b),
                    b'\\' | b'"' => out.push(byte),
                    b'0'..=b'7' => {
                        let mut value = (byte - b'0') as u32;
                        let mut used = 1;
                        while used < 3 {
                            let Some(&next) = bytes.get(i + 1) else {
                                break;
                            };
                            if !(b'0'..=b'7').contains(&next) {
                                break;
                            }
                            value = value * 8 + (next - b'0') as u32;
                            i += 1;
                            used += 1;
                        }
                        out.push(value as u8);
                    }
                    other => out.push(other),
                }
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum DiffFailed {
    #[error(
        "Git isn't installed, so there is no diff to show. Install Git (https://git-scm.com/downloads), then run `leaf diff` again."
    )]
    GitMissing,
    #[error(
        "{path} isn't inside a Git repository, so there is no diff to show. `leaf diff` compares files tracked by Git; run it inside a repository, or pass a path that is."
    )]
    NotRepo { path: PathBuf },
    #[error("`{command}` failed: {cause}")]
    Git { command: String, cause: String },
    #[error(
        "The diff is larger than {limit} MB, so leaf stopped loading it. Narrow it with a path or a revision range, then try again."
    )]
    TooLarge { limit: usize },
    #[error(
        "Could not read {path} ({cause}). Check that the file exists and is readable, then try again."
    )]
    Read { path: PathBuf, cause: String },
    #[error(
        "No base branch to compare with: the repository has no origin/HEAD, origin/main, main, origin/master or master. Pass `leaf diff --base <ref>` naming what to compare against."
    )]
    NoBase,
    #[error(transparent)]
    Github(#[from] github::GithubFailed),
}

/// A Git repository, found from any path inside it.
#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
}

impl Repo {
    /// The repository holding `start`, which may be a file or a directory.
    pub fn discover(start: &Path) -> Result<Self, DiffFailed> {
        let start = if start.is_file() {
            start.parent().unwrap_or(start)
        } else {
            start
        };
        let output = Command::new("git")
            .arg("-C")
            .arg(start)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => DiffFailed::GitMissing,
                _ => DiffFailed::Git {
                    command: "git rev-parse --show-toplevel".to_owned(),
                    cause: e.to_string(),
                },
            })?;
        if !output.status.success() {
            return Err(DiffFailed::NotRepo {
                path: start.to_owned(),
            });
        }
        let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let root = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(root));
        Ok(Repo { root })
    }

    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root.display().to_string())
    }

    pub fn branch(&self) -> Option<String> {
        let branch = self.git_text(&["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
        let branch = branch.trim();
        (branch != "HEAD" && !branch.is_empty()).then(|| branch.to_owned())
    }

    pub fn has_head(&self) -> bool {
        self.git_text(&["rev-parse", "--verify", "--quiet", "HEAD"])
            .is_ok()
    }

    /// Whether `spec` names a branch or commit here.
    pub fn has_rev(&self, spec: &str) -> bool {
        !spec.is_empty()
            && self
                .git_text(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{spec}^{{commit}}"),
                ])
                .is_ok()
    }

    /// The branch this one is normally compared with.
    pub fn base(&self) -> Option<String> {
        if let Ok(base) = self.git_text(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ]) {
            let base = base.trim();
            if !base.is_empty() {
                return Some(base.to_owned());
            }
        }
        ["origin/main", "origin/master", "main", "master"]
            .into_iter()
            .find(|base| {
                self.git_text(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{base}^{{commit}}"),
                ])
                .is_ok()
            })
            .map(str::to_owned)
    }

    pub fn working_patch(&self, paths: &[String]) -> Result<String, DiffFailed> {
        let mut patch = if self.has_head() {
            self.diff(&["diff", "HEAD"], paths)?
        } else {
            self.diff(&["diff", "--cached"], paths)?
        };
        patch.push_str(&self.untracked_patch(paths)?);
        Ok(patch)
    }

    pub fn staged_patch(&self, paths: &[String]) -> Result<String, DiffFailed> {
        if self.has_head() {
            self.diff(&["diff", "--cached", "HEAD"], paths)
        } else {
            self.diff(&["diff", "--cached"], paths)
        }
    }

    pub fn range_patch(&self, spec: &str, paths: &[String]) -> Result<String, DiffFailed> {
        self.diff(&["diff", spec], paths)
    }

    /// Changes on this branch since it parted from `base`, and the base's name.
    pub fn branch_patch(
        &self,
        base: Option<&str>,
        paths: &[String],
    ) -> Result<(String, String), DiffFailed> {
        let base = match base {
            Some(base) => base.to_owned(),
            None => self.base().ok_or(DiffFailed::NoBase)?,
        };
        if self
            .git_text(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{base}^{{commit}}"),
            ])
            .is_err()
        {
            return Err(DiffFailed::Git {
                command: format!("git rev-parse {base}"),
                cause: format!("{base} is not a branch or commit in this repository"),
            });
        }
        let merge_base =
            self.git_text(&["merge-base", &base, "HEAD"])
                .map_err(|_| DiffFailed::Git {
                    command: format!("git merge-base {base} HEAD"),
                    cause: format!("this branch and {base} have no common ancestor"),
                })?;
        let patch = self.diff(&["diff", merge_base.trim()], paths)?;
        Ok((patch, base))
    }

    fn diff(&self, base: &[&str], paths: &[String]) -> Result<String, DiffFailed> {
        let mut args: Vec<String> = base.iter().map(|arg| (*arg).to_owned()).collect();
        args.extend(
            [
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--find-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ]
            .map(str::to_owned),
        );
        if !paths.is_empty() {
            args.push("--".to_owned());
            args.extend(paths.iter().cloned());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git_text(&refs)
    }

    /// Text for files Git doesn't track yet, made into new-file patches.
    fn untracked_patch(&self, paths: &[String]) -> Result<String, DiffFailed> {
        let list = self.git_bytes(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        let mut patch = String::new();
        for raw in list.split(|byte| *byte == 0) {
            if raw.is_empty() {
                continue;
            }
            let path = String::from_utf8_lossy(raw);
            if !paths.is_empty()
                && !paths
                    .iter()
                    .any(|p| path == *p || path.starts_with(&format!("{p}/")))
            {
                continue;
            }
            synth_untracked(&self.root, &path, &mut patch);
        }
        Ok(patch)
    }

    fn git_text(&self, args: &[&str]) -> Result<String, DiffFailed> {
        self.git_bytes(args)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>, DiffFailed> {
        let command = format!("git -C {} {}", self.root.display(), args.join(" "));
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => DiffFailed::GitMissing,
                _ => DiffFailed::Git {
                    command: command.clone(),
                    cause: e.to_string(),
                },
            })?;

        let mut stderr = child.stderr.take();
        let errors = std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(stderr) = stderr.as_mut() {
                let mut bytes = Vec::new();
                let _ = stderr.take(64 * 1024).read_to_end(&mut bytes);
                text = String::from_utf8_lossy(&bytes).trim().to_owned();
            }
            text
        });

        let mut stdout = child.stdout.take();
        let mut bytes = Vec::new();
        let too_large = match stdout.as_mut() {
            Some(stdout) => stdout
                .take(MAX_PATCH_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| DiffFailed::Git {
                    command: command.clone(),
                    cause: e.to_string(),
                })?,
            None => 0,
        } > MAX_PATCH_BYTES;
        if too_large {
            let _ = child.kill();
        }
        let status = child.wait().map_err(|e| DiffFailed::Git {
            command: command.clone(),
            cause: e.to_string(),
        })?;
        let cause = errors.join().unwrap_or_default();
        if too_large {
            return Err(DiffFailed::TooLarge {
                limit: MAX_PATCH_BYTES / (1024 * 1024),
            });
        }
        if !status.success() {
            return Err(DiffFailed::Git {
                command,
                cause: if cause.is_empty() {
                    format!("exited with {status}")
                } else {
                    cause
                },
            });
        }
        Ok(bytes)
    }
}

/// Adds a new-file patch for `path`, which Git doesn't track yet.
fn synth_untracked(root: &Path, path: &str, patch: &mut String) {
    let full = root.join(path);
    let Ok(metadata) = std::fs::metadata(&full) else {
        return;
    };
    if !metadata.is_file() {
        return;
    }
    let old = quote_path(&format!("a/{path}"));
    let new = quote_path(&format!("b/{path}"));
    let _ = writeln!(patch, "diff --git {old} {new}");
    let _ = writeln!(patch, "new file mode 100644");
    if metadata.len() > MAX_UNTRACKED_BYTES {
        let _ = writeln!(
            patch,
            "leaf: {} MB file, too large to show",
            metadata.len() / (1024 * 1024)
        );
        return;
    }
    let Ok(bytes) = std::fs::read(&full) else {
        return;
    };
    if bytes.iter().take(SNIFF_BYTES).any(|byte| *byte == 0) {
        let _ = writeln!(patch, "Binary files {old} and {new} differ");
        return;
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return;
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let _ = writeln!(patch, "@@ -0,0 +1,{} @@", lines.len());
    for line in &lines {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let _ = writeln!(patch, "+{line}");
    }
    if !text.ends_with('\n') {
        let _ = writeln!(patch, "\\ No newline at end of file");
    }
}

/// Quotes a path the way Git does when it contains awkward bytes.
fn quote_path(path: &str) -> String {
    if !path
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\' || c.is_whitespace())
    {
        return path.to_owned();
    }
    let mut out = String::from("\"");
    for &byte in path.as_bytes() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(byte as char),
            _ => {
                let _ = write!(out, "\\{byte:03o}");
            }
        }
    }
    out.push('"');
    out
}

/// Where a diff comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The working tree against the last commit, including untracked files.
    Working,
    /// The index against the last commit.
    Staged,
    /// This branch since it parted from a base branch.
    Branch { base: Option<String> },
    /// Any revision or range Git understands.
    Range { spec: String },
    /// A `.diff` or `.patch` file.
    Patch { path: PathBuf },
    /// A GitHub pull request, commit or compare page.
    Github { url: String },
}

impl Origin {
    pub fn label(&self) -> String {
        match self {
            Origin::Working => "uncommitted changes".to_owned(),
            Origin::Staged => "staged changes".to_owned(),
            Origin::Branch { base: Some(base) } => format!("branch vs {base}"),
            Origin::Branch { base: None } => "branch changes".to_owned(),
            Origin::Range { spec } => spec.clone(),
            Origin::Patch { path } => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Origin::Github { url } => github::Resource::parse_url(url)
                .map(|resource| resource.label())
                .unwrap_or_else(|| url.clone()),
        }
    }
}

/// A diff to open, and the repository and paths it applies to.
#[derive(Debug, Clone)]
pub struct Request {
    pub origin: Origin,
    pub repo: Option<Repo>,
    pub paths: Vec<String>,
}

impl Request {
    pub fn label(&self) -> String {
        self.origin.label()
    }

    /// The same local changes seen the other way: branch, or working tree.
    pub fn other_side(&self) -> Option<Request> {
        let origin = match &self.origin {
            Origin::Working | Origin::Staged => Origin::Branch { base: None },
            Origin::Branch { .. } => Origin::Working,
            _ => return None,
        };
        Some(Request {
            origin,
            repo: self.repo.clone(),
            paths: self.paths.clone(),
        })
    }

    pub fn load(&self) -> Result<Loaded, DiffFailed> {
        match &self.origin {
            Origin::Github { url } => {
                let fetched = github::fetch(url)?;
                let key = format!("diff:github:{}", fetched.resource.key());
                let label = fetched.resource.label();
                Ok(Loaded {
                    diff: Diff::parse(fetched.patch),
                    title: format!("leaf diff · {label}"),
                    key,
                    label,
                    request: self.clone(),
                    github: Some(fetched.resource),
                    branch: None,
                })
            }
            Origin::Patch { path } => {
                let bytes = std::fs::read(path).map_err(|e| DiffFailed::Read {
                    path: path.clone(),
                    cause: e.to_string(),
                })?;
                let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                Ok(Loaded {
                    diff: Diff::parse(String::from_utf8_lossy(&bytes).into_owned()),
                    title: format!("leaf diff · {label}"),
                    key: format!("diff:patch:{}", canonical.display()),
                    label,
                    request: self.clone(),
                    github: None,
                    branch: None,
                })
            }
            local => {
                let repo = self.repo.as_ref().ok_or_else(|| DiffFailed::NotRepo {
                    path: std::env::current_dir().unwrap_or_default(),
                })?;
                let (patch, label, key) = match local {
                    Origin::Working => (
                        repo.working_patch(&self.paths)?,
                        "uncommitted changes".to_owned(),
                        "working".to_owned(),
                    ),
                    Origin::Staged => (
                        repo.staged_patch(&self.paths)?,
                        "staged changes".to_owned(),
                        "staged".to_owned(),
                    ),
                    Origin::Branch { base } => {
                        let (patch, resolved) = repo.branch_patch(base.as_deref(), &self.paths)?;
                        (
                            patch,
                            format!("branch vs {resolved}"),
                            format!("branch:{resolved}"),
                        )
                    }
                    Origin::Range { spec } => (
                        repo.range_patch(spec, &self.paths)?,
                        spec.clone(),
                        format!("range:{spec}"),
                    ),
                    Origin::Github { .. } | Origin::Patch { .. } => unreachable!(),
                };
                Ok(Loaded {
                    diff: Diff::parse(patch),
                    title: format!("leaf diff · {}", repo.name()),
                    key: format!("diff:git:{}:{key}", repo.root.display()),
                    label,
                    request: self.clone(),
                    github: None,
                    branch: repo.branch(),
                })
            }
        }
    }
}

/// A patch ready to show, with where it came from and what it belongs to.
pub struct Loaded {
    pub diff: Diff,
    pub title: String,
    /// The key saved comments hang off.
    pub key: String,
    pub label: String,
    pub request: Request,
    pub github: Option<github::Resource>,
    pub branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,3 +10,4 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 }
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-Hello
+Hello, world
";

    #[test]
    fn parses_files_hunks_and_line_numbers() {
        let diff = Diff::parse(SIMPLE.to_owned());
        assert_eq!(diff.files().len(), 2);
        assert_eq!(diff.files()[0].path(), "src/main.rs");
        assert_eq!(diff.files()[0].added, 2);
        assert_eq!(diff.files()[0].removed, 1);
        assert_eq!(diff.files()[0].hunks.len(), 1);

        let rows = diff.rows();
        assert_eq!(rows[0].kind, Kind::File);
        assert_eq!(rows[1].kind, Kind::Hunk);
        assert_eq!((rows[2].old, rows[2].new), (10, 10));
        assert_eq!(rows[3].kind, Kind::Removed);
        assert_eq!((rows[3].old, rows[3].new), (11, 0));
        assert_eq!(rows[4].kind, Kind::Added);
        assert_eq!((rows[4].old, rows[4].new), (0, 11));
        assert_eq!((rows[5].old, rows[5].new), (0, 12));
        assert_eq!((rows[6].old, rows[6].new), (12, 13));
        assert_eq!(diff.row_text(3), "-let b = 2;");
        assert_eq!(diff.content(5), "let c = 4;");
        assert_eq!(diff.max_line(), 13);
    }

    #[test]
    fn a_selection_becomes_a_github_anchor_and_a_snippet() {
        let diff = Diff::parse(SIMPLE.to_owned());
        let (first, last, anchor) = diff.comment_range(3, 4).expect("a range");
        assert_eq!((first, last), (3, 4));
        assert_eq!(anchor.file, "src/main.rs");
        assert_eq!(anchor.side, Side::New);
        assert_eq!((anchor.start_line, anchor.end_line), (11, 11));
        assert_eq!(diff.snippet(first, last), "-let b = 2;\n+let b = 3;");

        let (_, _, anchor) = diff.comment_range(3, 3).expect("one removed line");
        assert_eq!(anchor.side, Side::Old);
        assert_eq!((anchor.start_line, anchor.end_line), (11, 11));

        let (_, _, anchor) = diff.comment_range(2, 6).expect("a block");
        assert_eq!((anchor.start_line, anchor.end_line), (10, 13));
    }

    #[test]
    fn a_selection_that_crosses_hunks_keeps_the_last_one() {
        let patch = "\
diff --git a/a b/a
--- a/a
+++ b/a
@@ -1,2 +1,2 @@
-one
+ONE
 two
@@ -10,2 +10,2 @@
-three
+THREE
 four
";
        let diff = Diff::parse(patch.to_owned());
        let (first, last, anchor) = diff.comment_range(2, 8).expect("a range");
        assert_eq!(diff.row_text(first), "-three");
        assert_eq!(diff.row_text(last), " four");
        assert_eq!((anchor.start_line, anchor.end_line), (10, 11));
    }

    #[test]
    fn renames_binaries_and_missing_newlines_parse() {
        let patch = "\
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
diff --git a/logo.png b/logo.png
new file mode 100644
Binary files /dev/null and b/logo.png differ
diff --git a/x.txt b/x.txt
--- a/x.txt
+++ b/x.txt
@@ -1,2 +1,2 @@
 a
-b
\\ No newline at end of file
+c
\\ No newline at end of file
";
        let diff = Diff::parse(patch.to_owned());
        assert_eq!(diff.files()[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!(diff.files()[0].new_path.as_deref(), Some("new.txt"));
        assert_eq!(diff.files()[0].display(), "old.txt → new.txt");
        assert!(diff.files()[1].binary);
        assert_eq!(
            diff.files()[1].note.as_deref(),
            Some("binary file, not shown")
        );
        let kinds: Vec<Kind> = diff.rows().iter().map(|row| row.kind).collect();
        assert_eq!(kinds.iter().filter(|k| **k == Kind::NoNewline).count(), 2);
    }

    #[test]
    fn quoted_paths_round_trip() {
        let patch = "\
diff --git \"a/a b.txt\" \"b/a b.txt\"
new file mode 100644
--- /dev/null
+++ \"b/a b.txt\"
@@ -0,0 +1 @@
+hi
";
        let diff = Diff::parse(patch.to_owned());
        assert_eq!(diff.files()[0].old_path, None);
        assert_eq!(diff.files()[0].new_path.as_deref(), Some("a b.txt"));
        assert_eq!(diff.files()[0].added, 1);
        let quoted = quote_path("a/a b.txt");
        assert_eq!(quoted, "\"a/a b.txt\"");
        assert_eq!(
            unquote(&quoted).map(|(path, _)| path).as_deref(),
            Some("a/a b.txt")
        );
    }

    #[test]
    fn a_file_header_without_a_diff_line_still_opens() {
        let patch = "--- a/notes.md\n+++ b/notes.md\n@@ -1 +1 @@\n-a\n+b\n";
        let diff = Diff::parse(patch.to_owned());
        assert_eq!(diff.files().len(), 1);
        assert_eq!(diff.files()[0].path(), "notes.md");
        assert_eq!(diff.files()[0].added, 1);
        assert_eq!(diff.files()[0].removed, 1);
    }

    #[test]
    fn finding_a_line_after_a_reload_falls_back_to_the_file() {
        let diff = Diff::parse(SIMPLE.to_owned());
        assert_eq!(diff.row_for("src/main.rs", Side::New, 12), Some(5));
        assert_eq!(diff.row_for("src/main.rs", Side::New, 999), Some(2));
        assert_eq!(diff.row_for("other/main.rs", Side::New, 12), Some(2));
        assert_eq!(diff.row_for("nowhere.txt", Side::New, 12), None);
    }
}
