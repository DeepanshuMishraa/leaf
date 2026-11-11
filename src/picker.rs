//! `leaf open` with nothing named: browse for a document, the way Rex's
//! Go to Directory does. The field holds a path; what follows its last `/`
//! narrows that folder's folders and documents. Enter or tab on a folder goes
//! into it; enter on a document opens it. Documents read lately come first
//! while the field is untouched. A link typed out opens as it is.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use crate::popover::{self, Edit, Row, Section};

const READABLE: [&str; 4] = ["pdf", "epub", "md", "markdown"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Folder(PathBuf),
    Document(PathBuf),
    /// Read before: a path or a link, and how far in.
    Recent {
        target: String,
        percent: usize,
    },
    Link(String),
}

impl Entry {
    fn row(&self, here: &Path) -> Row {
        let name = |path: &Path| {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        };
        match self {
            Entry::Folder(path) => Row {
                icon: "📁",
                label: name(path),
                detail: String::new(),
                checked: false,
            },
            Entry::Document(path) => Row {
                icon: "📄",
                label: name(path),
                detail: String::new(),
                checked: false,
            },
            Entry::Recent { target, percent } => Row {
                icon: if is_link(target) { "🌐" } else { "📄" },
                label: Path::new(target)
                    .strip_prefix(here)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| tilde(target)),
                detail: format!("{percent}%"),
                checked: false,
            },
            Entry::Link(url) => Row {
                icon: "🌐",
                label: format!("Read {url}"),
                detail: String::new(),
                checked: false,
            },
        }
    }
}

fn readable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| READABLE.iter().any(|r| e.eq_ignore_ascii_case(r)))
}

fn is_link(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

fn expand(typed: &str) -> PathBuf {
    match typed.strip_prefix('~') {
        Some(rest) => PathBuf::from(format!(
            "{}{rest}",
            std::env::var("HOME").unwrap_or_default()
        )),
        None => PathBuf::from(typed),
    }
}

/// The folder a field names, and the text narrowing it.
pub fn split(typed: &str) -> (PathBuf, String) {
    match typed.rfind('/') {
        Some(at) => (expand(&typed[..=at]), typed[at + 1..].to_owned()),
        None => (PathBuf::from("."), typed.to_owned()),
    }
}

/// How well `name` answers `filter`: a start beats a middle beats letters in order.
fn score(name: &str, filter: &str) -> Option<u8> {
    if filter.is_empty() {
        return Some(0);
    }
    let (name, filter) = (name.to_lowercase(), filter.to_lowercase());
    if name.starts_with(&filter) {
        Some(0)
    } else if name.contains(&filter) {
        Some(1)
    } else {
        let mut letters = name.chars();
        filter.chars().all(|c| letters.any(|l| l == c)).then_some(2)
    }
}

/// The folders, then the documents, in `dir` that answer `filter`; hidden ones only when asked for.
pub fn children(dir: &Path, filter: &str) -> Vec<Entry> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(bool, u8, String, Entry)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !filter.starts_with('.') {
                return None;
            }
            let path = entry.path();
            // Follows links, so a linked folder is a folder.
            let folder = path.is_dir();
            if !folder && !readable(&path) {
                return None;
            }
            let rank = score(&name, filter)?;
            let entry = if folder {
                Entry::Folder(path)
            } else {
                Entry::Document(path)
            };
            Some((!folder, rank, name.to_lowercase(), entry))
        })
        .collect();
    found.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    found.into_iter().map(|(_, _, _, entry)| entry).collect()
}

struct Picker {
    start: String,
    typed: String,
    selected: usize,
    recent: Vec<(String, usize)>,
}

impl Picker {
    /// Recent documents while the field is as it started, then what's in the folder.
    fn entries(&self) -> Vec<(String, Vec<Entry>)> {
        if is_link(&self.typed) {
            return vec![(
                String::new(),
                vec![Entry::Link(self.typed.trim().to_owned())],
            )];
        }
        let (dir, filter) = split(&self.typed);
        let mut out = Vec::new();
        if self.typed == self.start && !self.recent.is_empty() {
            out.push((
                "Recent".to_owned(),
                self.recent
                    .iter()
                    .filter(|(key, _)| is_link(key) || Path::new(key).is_file())
                    .take(5)
                    .map(|(target, percent)| Entry::Recent {
                        target: target.clone(),
                        percent: *percent,
                    })
                    .collect(),
            ));
        }
        out.push(("Go to".to_owned(), children(&dir, &filter)));
        out
    }

    fn sections(&self, entries: &[(String, Vec<Entry>)]) -> Vec<Section> {
        let (here, _) = split(&self.start);
        entries
            .iter()
            .map(|(title, list)| Section {
                title: title.clone(),
                rows: list.iter().map(|e| e.row(&here)).collect(),
            })
            .collect()
    }

    /// Enter: a document to read, or `None` after going into a folder.
    fn choose(&mut self, entry: Option<Entry>) -> Option<String> {
        match entry {
            Some(Entry::Folder(path)) => {
                self.typed = format!("{}/", path.display());
                self.selected = 0;
                None
            }
            Some(Entry::Document(path)) => Some(path.display().to_string()),
            Some(Entry::Recent { target, .. }) => Some(target),
            Some(Entry::Link(url)) => Some(url),
            None => {
                let path = expand(self.typed.trim());
                path.is_file().then(|| path.display().to_string())
            }
        }
    }
}

/// Shows the picker until a document is chosen, or `None` when it's closed.
pub fn pick(root: &Path, recent: Vec<(String, usize)>) -> io::Result<Option<String>> {
    let start = format!("{}/", root.display().to_string().trim_end_matches('/'));
    let mut picker = Picker {
        typed: start.clone(),
        start,
        selected: 0,
        recent,
    };
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle("Open"));
    let result = (|| loop {
        let entries = picker.entries();
        let sections = picker.sections(&entries);
        let rows = popover::count(&sections);
        picker.selected = picker.selected.min(rows.saturating_sub(1));
        terminal.draw(|f| {
            popover::draw(
                f,
                &popover::View {
                    chip: "Open",
                    typed: &picker.typed,
                    placeholder: "A path, or an https:// link…",
                    sections: &sections,
                    selected: picker.selected,
                    empty: "No folders or documents here. Type a path or a link.",
                    footer: "enter open · tab go into · esc close",
                },
            )
        })?;
        if !event::poll(Duration::from_secs(60))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let picked = entries
            .iter()
            .flat_map(|(_, list)| list)
            .nth(picker.selected)
            .cloned();
        match popover::edit(
            key.code,
            key.modifiers,
            &mut picker.typed,
            &mut picker.selected,
            rows,
        ) {
            Edit::Quit => return Ok(None),
            Edit::Choose => {
                if let Some(target) = picker.choose(picked) {
                    return Ok(Some(target));
                }
            }
            Edit::Complete => {
                if let Some(Entry::Folder(_)) = picked {
                    picker.choose(picked);
                }
            }
            Edit::Typed | Edit::Moved | Edit::Nothing => {}
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-picker-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("books")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("notes.md"), "# A").unwrap();
        std::fs::write(dir.join("bookmarks.pdf"), "").unwrap();
        std::fs::write(dir.join("build.rs"), "").unwrap();
        dir
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Folder(p) | Entry::Document(p) => {
                    p.file_name().unwrap().to_string_lossy().into_owned()
                }
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn folders_come_first_then_documents_and_the_rest_is_left_out() {
        let dir = tree("children");
        assert_eq!(
            names(&children(&dir, "")),
            ["books", "bookmarks.pdf", "notes.md"]
        );
        assert_eq!(names(&children(&dir, "boo")), ["books", "bookmarks.pdf"]);
        assert_eq!(names(&children(&dir, "nts")), ["notes.md"]);
        assert_eq!(names(&children(&dir, ".g")), [".git"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn enter_goes_into_folders_and_opens_documents() {
        let dir = tree("enter");
        let start = format!("{}/", dir.display());
        let mut picker = Picker {
            typed: start.clone(),
            start,
            selected: 0,
            recent: vec![
                (dir.join("notes.md").display().to_string(), 40),
                ("/gone.pdf".into(), 3),
            ],
        };
        let entries = picker.entries();
        assert_eq!(entries[0].0, "Recent");
        assert_eq!(
            entries[0].1.len(),
            1,
            "a recent file that's gone is left out"
        );
        assert_eq!(picker.choose(Some(Entry::Folder(dir.join("books")))), None);
        assert_eq!(picker.typed, format!("{}/", dir.join("books").display()));
        assert_eq!(
            picker.entries().len(),
            1,
            "recent only shows where it started"
        );
        picker.typed = "https://example.com/post".into();
        let entries = picker.entries();
        assert_eq!(
            picker.choose(entries[0].1.first().cloned()).as_deref(),
            Some("https://example.com/post")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_field_splits_into_a_folder_and_a_filter() {
        assert_eq!(split("/a/b/c"), (PathBuf::from("/a/b/"), "c".to_owned()));
        assert_eq!(split("/a/"), (PathBuf::from("/a/"), String::new()));
    }
}
