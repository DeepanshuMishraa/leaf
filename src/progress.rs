//! Saved reading places, highlights and the image mode, in SQLite.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::comment::{Anchor, Comment, Side, State};
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
        Progress {
            page,
            page_count,
            percent,
        }
    }
}

/// Where a reader is in a document without pages (a markdown file or an
/// article). `anchor` is the place in the page text of the line at the top of
/// the screen, so the place holds at any window width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    pub anchor: usize,
    pub percent: usize,
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
CREATE TABLE IF NOT EXISTS scroll (
  key TEXT PRIMARY KEY,
  anchor INTEGER NOT NULL,
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
CREATE INDEX IF NOT EXISTS highlights_key ON highlights (key);
CREATE TABLE IF NOT EXISTS bookmarks (
  key TEXT NOT NULL,
  page INTEGER NOT NULL,
  label TEXT NOT NULL,
  PRIMARY KEY (key, page)
);
CREATE TABLE IF NOT EXISTS diff_comments (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  key TEXT NOT NULL,
  file TEXT NOT NULL,
  side TEXT NOT NULL,
  start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL,
  body TEXT NOT NULL,
  state TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS diff_comments_key ON diff_comments (key)";

/// A page the reader marked to come back to. `page` is 0-based; `label` is
/// the start of the page's text, so the list says what's there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub page: usize,
    pub label: String,
}

/// A review comment and the row it can be changed or removed by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedComment {
    pub id: i64,
    pub comment: Comment,
}

/// A saved comment plus the diff key it belongs to, for `leaf comments`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedComment {
    pub key: String,
    pub saved: SavedComment,
}

/// `id, file, side, start_line, end_line, body, state` as stored in SQLite.
type CommentRow = (i64, String, String, i64, i64, String, String);

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
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".local").join("share")
        });
    data.join("leaf").join("leaf.db")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A stored count, or `None` if it isn't one we wrote.
fn count(value: i64) -> Option<usize> {
    usize::try_from(value).ok()
}

impl Store {
    /// Opens (creating if needed) the store at `path`; `:memory:` keeps it in memory.
    pub fn open(path: &Path) -> Result<Self, StoreFailed> {
        let failed = |cause: String| StoreFailed {
            operation: Operation::Open,
            path: path.to_owned(),
            cause,
        };
        let in_memory = path == Path::new(":memory:");
        if !in_memory && let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
        }
        let db = if in_memory {
            Connection::open_in_memory()
        } else {
            Connection::open(path)
        }
        .map_err(|e| failed(e.to_string()))?;
        db.execute_batch(SCHEMA)
            .map_err(|e| failed(e.to_string()))?;
        Ok(Store {
            db,
            path: path.to_owned(),
        })
    }

    fn failed(&self, operation: Operation) -> impl Fn(rusqlite::Error) -> StoreFailed + '_ {
        move |e| StoreFailed {
            operation,
            path: self.path.clone(),
            cause: e.to_string(),
        }
    }

    pub fn load(&self, key: &str) -> Result<Option<Progress>, StoreFailed> {
        let row = self
            .db
            .query_row(
                "SELECT page, page_count, percent FROM progress WHERE key = ?",
                [key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(self.failed(Operation::Read))?;
        Ok(row.and_then(|(page, page_count, percent)| {
            let progress = Progress {
                page: count(page)?,
                page_count: count(page_count)?,
                percent: count(percent)?,
            };
            (progress.page >= 1).then_some(progress)
        }))
    }

    pub fn save(&self, key: &str, progress: Progress) -> Result<(), StoreFailed> {
        self.db
            .execute(
                "INSERT INTO progress (key, page, page_count, percent, updated_at) VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET page = excluded.page, page_count = excluded.page_count,
                   percent = excluded.percent, updated_at = excluded.updated_at",
                params![key, progress.page as i64, progress.page_count as i64, progress.percent as i64, now()],
            )
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }

    pub fn load_scroll(&self, key: &str) -> Result<Option<Scroll>, StoreFailed> {
        let row = self
            .db
            .query_row(
                "SELECT anchor, percent FROM scroll WHERE key = ?",
                [key],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(self.failed(Operation::Read))?;
        Ok(row.and_then(|(anchor, percent)| {
            Some(Scroll {
                anchor: count(anchor)?,
                percent: count(percent)?,
            })
        }))
    }

    pub fn save_scroll(&self, key: &str, scroll: Scroll) -> Result<(), StoreFailed> {
        self.db
            .execute(
                "INSERT INTO scroll (key, anchor, percent, updated_at) VALUES (?, ?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET anchor = excluded.anchor, percent = excluded.percent, updated_at = excluded.updated_at",
                params![key, scroll.anchor as i64, scroll.percent as i64, now()],
            )
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }

    pub fn load_image_mode(&self) -> Result<Option<ImageMode>, StoreFailed> {
        let value: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM settings WHERE key = 'image_mode'",
                [],
                |r| r.get(0),
            )
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
            .prepare(
                "SELECT page, start, end, text FROM highlights WHERE key = ? ORDER BY page, start",
            )
            .map_err(self.failed(Operation::Read))?;
        let rows = query
            .query_map([key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
            .map_err(self.failed(Operation::Read))?;
        let mut highlights = Vec::new();
        for row in rows {
            let (page, start, end, text) = row.map_err(self.failed(Operation::Read))?;
            // Rows we didn't write are skipped rather than trusted.
            if let (Some(page), Some(start), Some(end)) = (count(page), count(start), count(end))
                && end > start
            {
                highlights.push(Highlight {
                    page,
                    start,
                    end,
                    text,
                });
            }
        }
        Ok(highlights)
    }

    /// Replaces every highlight saved for `key`.
    pub fn save_highlights(
        &mut self,
        key: &str,
        highlights: &[Highlight],
    ) -> Result<(), StoreFailed> {
        let path = self.path.clone();
        let failed = |e: rusqlite::Error| StoreFailed {
            operation: Operation::Save,
            path: path.clone(),
            cause: e.to_string(),
        };
        let tx = self.db.transaction().map_err(failed)?;
        tx.execute("DELETE FROM highlights WHERE key = ?", [key])
            .map_err(failed)?;
        for h in highlights {
            tx.execute(
                "INSERT INTO highlights (key, page, start, end, text) VALUES (?, ?, ?, ?, ?)",
                params![key, h.page as i64, h.start as i64, h.end as i64, h.text],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)
    }

    pub fn load_bookmarks(&self, key: &str) -> Result<Vec<Bookmark>, StoreFailed> {
        let mut query = self
            .db
            .prepare("SELECT page, label FROM bookmarks WHERE key = ? ORDER BY page")
            .map_err(self.failed(Operation::Read))?;
        let rows = query
            .query_map([key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map_err(self.failed(Operation::Read))?;
        let mut bookmarks = Vec::new();
        for row in rows {
            let (page, label) = row.map_err(self.failed(Operation::Read))?;
            if let Some(page) = count(page) {
                bookmarks.push(Bookmark { page, label });
            }
        }
        Ok(bookmarks)
    }

    /// Replaces every bookmark of `key` with `bookmarks`.
    pub fn save_bookmarks(&mut self, key: &str, bookmarks: &[Bookmark]) -> Result<(), StoreFailed> {
        let path = self.path.clone();
        let failed = |e: rusqlite::Error| StoreFailed {
            operation: Operation::Save,
            path: path.clone(),
            cause: e.to_string(),
        };
        let tx = self.db.transaction().map_err(failed)?;
        tx.execute("DELETE FROM bookmarks WHERE key = ?", [key])
            .map_err(failed)?;
        for b in bookmarks {
            tx.execute(
                "INSERT OR REPLACE INTO bookmarks (key, page, label) VALUES (?, ?, ?)",
                params![key, b.page as i64, b.label],
            )
            .map_err(failed)?;
        }
        tx.commit().map_err(failed)
    }

    pub fn load_diff_comments(&self, key: &str) -> Result<Vec<SavedComment>, StoreFailed> {
        let mut query = self
            .db
            .prepare(
                "SELECT id, file, side, start_line, end_line, body, state
                 FROM diff_comments WHERE key = ? ORDER BY id",
            )
            .map_err(self.failed(Operation::Read))?;
        let rows = query
            .query_map([key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(self.failed(Operation::Read))?;
        let mut comments = Vec::new();
        for row in rows {
            let row = row.map_err(self.failed(Operation::Read))?;
            if let Some(saved) = saved_comment(row) {
                comments.push(saved);
            }
        }
        Ok(comments)
    }

    /// Every saved comment with its diff key, oldest first per diff.
    pub fn load_all_diff_comments(&self) -> Result<Vec<KeyedComment>, StoreFailed> {
        let mut query = self
            .db
            .prepare(
                "SELECT key, id, file, side, start_line, end_line, body, state
                 FROM diff_comments ORDER BY key, id",
            )
            .map_err(self.failed(Operation::Read))?;
        let rows = query
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ),
                ))
            })
            .map_err(self.failed(Operation::Read))?;
        let mut comments = Vec::new();
        for row in rows {
            let (key, row) = row.map_err(self.failed(Operation::Read))?;
            if let Some(saved) = saved_comment(row) {
                comments.push(KeyedComment { key, saved });
            }
        }
        Ok(comments)
    }

    pub fn insert_diff_comment(&self, key: &str, comment: &Comment) -> Result<i64, StoreFailed> {
        self.db
            .execute(
                "INSERT INTO diff_comments
                   (key, file, side, start_line, end_line, body, state, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    key,
                    comment.anchor.file,
                    comment.anchor.side.as_str(),
                    comment.anchor.start_line as i64,
                    comment.anchor.end_line as i64,
                    comment.body,
                    comment.state.as_str(),
                    now()
                ],
            )
            .map_err(self.failed(Operation::Save))?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn set_diff_comment_state(&self, id: i64, state: State) -> Result<(), StoreFailed> {
        self.db
            .execute(
                "UPDATE diff_comments SET state = ? WHERE id = ?",
                params![state.as_str(), id],
            )
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }

    pub fn delete_diff_comment(&self, id: i64) -> Result<(), StoreFailed> {
        self.db
            .execute("DELETE FROM diff_comments WHERE id = ?", [id])
            .map(drop)
            .map_err(self.failed(Operation::Save))
    }
}

/// Parses a diff_comments row. Rows written by another version are skipped,
/// not guessed at.
fn saved_comment(row: CommentRow) -> Option<SavedComment> {
    let (id, file, side, start, end, body, state) = row;
    let (Some(side), Some(state), Some(start), Some(end)) = (
        Side::parse(&side),
        State::parse(&state),
        count(start),
        count(end),
    ) else {
        return None;
    };
    if start == 0 || end < start {
        return None;
    }
    Some(SavedComment {
        id,
        comment: Comment {
            anchor: Anchor {
                file,
                side,
                start_line: start as u32,
                end_line: end as u32,
            },
            body,
            state,
        },
    })
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
        assert_eq!(
            store.load("/a.pdf").unwrap(),
            Some(Progress {
                page: 15,
                page_count: 613,
                percent: 2
            })
        );
        store.save("/a.pdf", Progress::of(40, 613)).unwrap();
        assert_eq!(store.load("/a.pdf").unwrap().map(|p| p.page), Some(40));
    }

    #[test]
    fn a_scroll_place_comes_back_for_that_document_only() {
        let store = memory();
        assert_eq!(store.load_scroll("/a.md").unwrap(), None);
        store
            .save_scroll(
                "/a.md",
                Scroll {
                    anchor: 120,
                    percent: 7,
                },
            )
            .unwrap();
        store
            .save_scroll(
                "https://x.org/b",
                Scroll {
                    anchor: 5,
                    percent: 1,
                },
            )
            .unwrap();
        store
            .save_scroll(
                "/a.md",
                Scroll {
                    anchor: 900,
                    percent: 40,
                },
            )
            .unwrap();
        assert_eq!(
            store.load_scroll("/a.md").unwrap(),
            Some(Scroll {
                anchor: 900,
                percent: 40
            })
        );
        assert_eq!(store.load("/a.md").unwrap(), None);
    }

    #[test]
    fn a_store_that_cant_be_created_is_an_error() {
        let failure = Store::open(Path::new("/nonexistent-dir/leaf.db"))
            .err()
            .expect("open fails");
        assert_eq!(failure.operation, Operation::Open);
        assert!(
            failure
                .to_string()
                .contains("The document itself is unaffected")
        );
    }

    #[test]
    fn highlights_come_back_per_document_and_saving_replaces_them() {
        let mut store = memory();
        let first = Highlight {
            page: 2,
            start: 10,
            end: 15,
            text: "hello".into(),
        };
        let second = Highlight {
            page: 0,
            start: 0,
            end: 3,
            text: "abc".into(),
        };
        store
            .save_highlights("/a.pdf", &[first.clone(), second.clone()])
            .unwrap();
        store
            .save_highlights("/b.pdf", std::slice::from_ref(&first))
            .unwrap();
        assert_eq!(
            store.load_highlights("/a.pdf").unwrap(),
            vec![second.clone(), first.clone()]
        );
        store
            .save_highlights("/a.pdf", std::slice::from_ref(&second))
            .unwrap();
        assert_eq!(store.load_highlights("/a.pdf").unwrap(), vec![second]);
        assert_eq!(store.load_highlights("/b.pdf").unwrap(), vec![first]);
    }

    #[test]
    fn bookmarks_come_back_per_document_in_page_order() {
        let mut store = memory();
        let (late, early) = (
            Bookmark {
                page: 9,
                label: "Chapter 2".into(),
            },
            Bookmark {
                page: 1,
                label: "Preface".into(),
            },
        );
        store
            .save_bookmarks("/a.epub", &[late.clone(), early.clone()])
            .unwrap();
        store
            .save_bookmarks("/b.epub", std::slice::from_ref(&late))
            .unwrap();
        assert_eq!(
            store.load_bookmarks("/a.epub").unwrap(),
            vec![early.clone(), late.clone()]
        );
        store.save_bookmarks("/a.epub", &[]).unwrap();
        assert!(store.load_bookmarks("/a.epub").unwrap().is_empty());
        assert_eq!(store.load_bookmarks("/b.epub").unwrap(), vec![late]);
    }

    #[test]
    fn the_image_mode_is_remembered() {
        let store = memory();
        assert_eq!(store.load_image_mode().unwrap(), None);
        store.save_image_mode(ImageMode::Mono).unwrap();
        assert_eq!(store.load_image_mode().unwrap(), Some(ImageMode::Mono));
    }

    #[test]
    fn diff_comments_keep_their_anchor_and_state() {
        let store = memory();
        let comment = Comment {
            anchor: Anchor {
                file: "src/main.rs".into(),
                side: Side::New,
                start_line: 12,
                end_line: 14,
            },
            body: "Why skip?".into(),
            state: State::Draft,
        };
        let id = store.insert_diff_comment("diff:a", &comment).unwrap();
        assert_eq!(store.load_diff_comments("diff:a").unwrap().len(), 1);
        store.set_diff_comment_state(id, State::Github).unwrap();
        let saved = store.load_diff_comments("diff:a").unwrap();
        assert_eq!(saved[0].comment.state, State::Github);
        assert_eq!(saved[0].comment.label(), "src/main.rs L12-L14");
        assert!(store.load_diff_comments("diff:b").unwrap().is_empty());
        let all = store.load_all_diff_comments().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].key, "diff:a");
        assert_eq!(all[0].saved, saved[0]);
        store.delete_diff_comment(id).unwrap();
        assert!(store.load_diff_comments("diff:a").unwrap().is_empty());
        assert!(store.load_all_diff_comments().unwrap().is_empty());
    }
}
