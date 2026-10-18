//! Saved reading places, highlights and the image mode, in SQLite.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::highlight::Highlight;

/// Where a reader is in a paged document. `page` is 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub page: usize,
    pub page_count: usize,
    pub percent: usize,
}

impl Progress {
    /// Progress after reading through `page`: the last page is 100%.
    pub fn of(page: usize, page_count: usize) -> Self {
        let percent = (page * 100).checked_div(page_count).unwrap_or(0);
        Progress { page, page_count, percent }
    }
}

/// How figures draw: as the image itself, or painted in the terminal's text
/// color on a transparent ground.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageMode {
    Color,
    Mono,
}

impl ImageMode {
    fn as_str(self) -> &'static str {
        match self {
            ImageMode::Color => "color",
            ImageMode::Mono => "mono",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "color" => Some(ImageMode::Color),
            "mono" => Some(ImageMode::Mono),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Open,
    Read,
    Save,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Could not {verb} reading progress and highlights in {path} ({cause}). The document itself is unaffected. Check that the folder is writable, or delete the file to start fresh.",
    verb = match operation { Operation::Open => "open", Operation::Read => "read", Operation::Save => "save" },
    path = path.display()
)]
pub struct StoreFailed {
    pub operation: Operation,
    pub path: PathBuf,
    pub cause: String,
}

/// The page is saved on every turn; a write-ahead log makes that an append
/// without waiting on the disk. A crash still keeps it; a power cut may lose the last few.
const SCHEMA: &str = "PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS progress (
  key TEXT PRIMARY KEY,
  page INTEGER NOT NULL,
  page_count INTEGER NOT NULL,
  percent INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS highlights (
  key TEXT NOT NULL,
  page INTEGER NOT NULL,
  start INTEGER NOT NULL,
  end INTEGER NOT NULL,
  text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS highlights_key ON highlights (key)";

/// Saved reading places and highlights, per document key (a PDF's real path
/// or a URL), plus the image mode for every document.
pub struct Store {
    db: Connection,
    path: PathBuf,
}

/// `$XDG_DATA_HOME/leaf/leaf.db`, falling back to `~/.local/share`.
pub fn default_path() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local").join("share")
        });
    data.join("leaf").join("leaf.db")
}

/// A stored count, or `None` if it isn't one we wrote.
fn count(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

impl Store {
    /// Opens (creating if needed) the store at `path`; `:memory:` keeps it in memory.
    pub fn open(path: &Path) -> Result<Self, StoreFailed> {
        let failed = |cause: String| StoreFailed { operation: Operation::Open, path: path.to_owned(), cause };
        let in_memory = path == Path::new(":memory:");
        if !in_memory && let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
        }
        let db = if in_memory { Connection::open_in_memory() } else { Connection::open(path) }
            .map_err(|e| failed(e.to_string()))?;
        db.execute_batch(SCHEMA).map_err(|e| failed(e.to_string()))?;
        Ok(Store { db, path: path.to_owned() })
    }

    fn failed(&self, operation: Operation) -> impl Fn(rusqlite::Error) -> StoreFailed + '_ {
        move |e| StoreFailed { operation, path: self.path.clone(), cause: e.to_string() }
    }

    pub fn load(&self, key: &str) -> Result<Option<Progress>, StoreFailed> {
        let row = self
            .db
            .query_row("SELECT page, page_count, percent FROM progress WHERE key = ?", [key], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
            })
            .optional()
            .map_err(self.failed(Operation::Read))?;
        Ok(row.and_then(|(page, page_count, percent)| {
            let progress = Progress { page: count(page)?, page_count: count(page_count)?, percent: count(percent)? };
            (progress.page >= 1).then_some(progress)
        }))
    }

    pub fn save(&self, key: &str, progress: Progress) -> Result<(), StoreFailed> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        self.db
            .execute(
                "INSERT INTO progress (key, page, page_count, percent, updated_at) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET page = excluded.page, page_count = excluded.page_count,
                   percent = excluded.percent, updated_at = excluded.updated_at",
                params![key, progress.page as i64, progress.page_count as i64, progress.percent as i64, now],
            )
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }

    pub fn load_image_mode(&self) -> Result<Option<ImageMode>, StoreFailed> {
        let value: Option<String> = self
            .db
            .query_row("SELECT value FROM settings WHERE key = 'image_mode'", [], |r| r.get(0))
            .optional()
            .map_err(self.failed(Operation::Read))?;
        Ok(value.as_deref().and_then(ImageMode::parse))
    }

    pub fn save_image_mode(&self, mode: ImageMode) -> Result<(), StoreFailed> {
        self.db
            .execute(
                "INSERT INTO settings (key, value) VALUES ('image_mode', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [mode.as_str()],
            )
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }

    pub fn load_highlights(&self, key: &str) -> Result<Vec<Highlight>, StoreFailed> {
        let mut query = self
            .db
            .prepare("SELECT page, start, end, text FROM highlights WHERE key = ? ORDER BY page, start")
            .map_err(self.failed(Operation::Read))?;
        let rows = query
            .query_map([key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?)))
            .map_err(self.failed(Operation::Read))?;
        let mut highlights = Vec::new();
        for row in rows {
            let (page, start, end, text) = row.map_err(self.failed(Operation::Read))?;
            // Rows we didn't write are skipped rather than trusted.
            if let (Some(page), Some(start), Some(end)) = (count(page), count(start), count(end))
                && end > start
            {
                highlights.push(Highlight { page, start, end, text });
            }
        }
        Ok(highlights)
    }

    /// Replaces every highlight saved for `key`.
    pub fn save_highlights(&mut self, key: &str, highlights: &[Highlight]) -> Result<(), StoreFailed> {
        let path = self.path.clone();
        let failed = |e: rusqlite::Error| StoreFailed { operation: Operation::Save, path: path.clone(), cause: e.to_string() };
        let tx = self.db.transaction().map_err(failed)?;
        tx.execute("DELETE FROM highlights WHERE key = ?", [key]).map_err(failed)?;
        for h in highlights {
            tx.execute(
                "INSERT INTO highlights (key, page, start, end, text) VALUES (?, ?, ?, ?, ?)",
                params![key, h.page as i64, h.start as i64, h.end as i64, h.text],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Store {
        Store::open(Path::new(":memory:")).expect("in-memory store opens")
    }

    #[test]
    fn percent_is_pages_done_out_of_the_whole() {
        assert_eq!(Progress::of(1, 613).percent, 0);
        assert_eq!(Progress::of(15, 613).percent, 2);
        assert_eq!(Progress::of(613, 613).percent, 100);
    }

    #[test]
    fn a_saved_place_comes_back_for_that_file_only_and_saving_replaces_it() {
        let store = memory();
        assert_eq!(store.load("/a.pdf").unwrap(), None);
        store.save("/a.pdf", Progress::of(15, 613)).unwrap();
        store.save("/b.pdf", Progress::of(3, 10)).unwrap();
        assert_eq!(store.load("/a.pdf").unwrap(), Some(Progress { page: 15, page_count: 613, percent: 2 }));
        store.save("/a.pdf", Progress::of(40, 613)).unwrap();
        assert_eq!(store.load("/a.pdf").unwrap().map(|p| p.page), Some(40));
    }

    #[test]
    fn a_store_that_cant_be_created_is_an_error() {
        let failure = Store::open(Path::new("/nonexistent-dir/leaf.db")).err().expect("open fails");
        assert_eq!(failure.operation, Operation::Open);
        assert!(failure.to_string().contains("The document itself is unaffected"));
    }

    #[test]
    fn highlights_come_back_per_document_and_saving_replaces_them() {
        let mut store = memory();
        let first = Highlight { page: 2, start: 10, end: 15, text: "hello".into() };
        let second = Highlight { page: 0, start: 0, end: 3, text: "abc".into() };
        store.save_highlights("/a.pdf", &[first.clone(), second.clone()]).unwrap();
        store.save_highlights("/b.pdf", std::slice::from_ref(&first)).unwrap();
        assert_eq!(store.load_highlights("/a.pdf").unwrap(), vec![second.clone(), first.clone()]);
        store.save_highlights("/a.pdf", std::slice::from_ref(&second)).unwrap();
        assert_eq!(store.load_highlights("/a.pdf").unwrap(), vec![second]);
        assert_eq!(store.load_highlights("/b.pdf").unwrap(), vec![first]);
    }

    #[test]
    fn the_image_mode_is_remembered() {
        let store = memory();
        assert_eq!(store.load_image_mode().unwrap(), None);
        store.save_image_mode(ImageMode::Mono).unwrap();
        assert_eq!(store.load_image_mode().unwrap(), Some(ImageMode::Mono));
    }
}
