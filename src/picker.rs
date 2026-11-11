//! `leaf open` with nothing named: pick a document to read.
//!
//! Lists the documents read lately, then every PDF, EPUB and markdown file
//! under the current directory. Typing narrows the list; a link or a path
//! typed out in full can be opened as it is.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::render::DIM;

const READABLE: [&str; 4] = ["pdf", "epub", "md", "markdown"];
/// Folders that hold tools' files, not documents.
const SKIPPED: [&str; 6] = [
    "node_modules",
    "target",
    "vendor",
    "Library",
    "build",
    "dist",
];
const DEEPEST: usize = 6;
const MOST_FILES: usize = 5000;
const MOST_FOLDERS: usize = 4000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What `leaf open` is given: a path or a link.
    pub target: String,
    /// What the list shows: relative to the folder, or the link.
    pub shown: String,
    /// How far in, for a document read before.
    pub percent: Option<usize>,
}

/// Every readable file under `root`, nearest first, skipping hidden and tool folders.
pub fn walk(root: &Path) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut folders = std::collections::VecDeque::from([(root.to_path_buf(), 0)]);
    let mut seen = 0;
    while let Some((dir, depth)) = folders.pop_front() {
        seen += 1;
        if seen > MOST_FOLDERS || found.len() >= MOST_FILES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < DEEPEST && !SKIPPED.contains(&name.as_str()) {
                    folders.push_back((path, depth + 1));
                }
            } else if readable(&path) {
                let shown = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                found.push(Candidate {
                    target: path.display().to_string(),
                    shown,
                    percent: None,
                });
            }
        }
    }
    found
}

fn readable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| READABLE.iter().any(|r| e.eq_ignore_ascii_case(r)))
}

/// The documents read lately that are still there, then `files` not among them.
pub fn merge(recent: Vec<(String, usize)>, root: &Path, files: Vec<Candidate>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = recent
        .into_iter()
        .filter(|(key, _)| is_link(key) || Path::new(key).is_file())
        .map(|(key, percent)| Candidate {
            shown: Path::new(&key)
                .strip_prefix(root)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| tilde(&key)),
            target: key,
            percent: Some(percent),
        })
        .collect();
    for file in files {
        if !out.iter().any(|seen| seen.target == file.target) {
            out.push(file);
        }
    }
    out
}

fn tilde(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_owned(),
    }
}

fn is_link(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// The candidates matching every typed word, best first. A word in the file
/// name beats one elsewhere in the path, which beats letters in order.
pub fn rank<'a>(candidates: &'a [Candidate], typed: &str) -> Vec<&'a Candidate> {
    let terms: Vec<String> = typed.split_whitespace().map(str::to_lowercase).collect();
    let mut scored: Vec<(usize, usize, &Candidate)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(at, candidate)| {
            let shown = candidate.shown.to_lowercase();
            let name = shown.rsplit('/').next().unwrap_or(&shown).to_owned();
            let score = terms.iter().try_fold(0, |score, term| {
                if name.contains(term.as_str()) {
                    Some(score)
                } else if shown.contains(term.as_str()) {
                    Some(score + 1)
                } else if in_order(&shown, term) {
                    Some(score + 3)
                } else {
                    None
                }
            })?;
            Some((score, at, candidate))
        })
        .collect();
    scored.sort_by_key(|(score, at, _)| (*score, *at));
    scored.into_iter().map(|(_, _, c)| c).collect()
}

fn in_order(hay: &str, term: &str) -> bool {
    let mut hay = hay.chars();
    term.chars().all(|c| hay.any(|h| h == c))
}

/// What enter opens: the picked row, or what was typed when it's a link or a file.
pub fn choice(ranked: &[&Candidate], selected: usize, typed: &str, root: &Path) -> Option<String> {
    let typed = typed.trim();
    if is_link(typed) {
        return Some(typed.to_owned());
    }
    if let Some(picked) = ranked.get(selected) {
        return Some(picked.target.clone());
    }
    let path = expand(typed, root);
    (!typed.is_empty() && path.is_file()).then(|| path.display().to_string())
}

fn expand(typed: &str, root: &Path) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    match typed.strip_prefix("~/") {
        Some(rest) if !home.is_empty() => Path::new(&home).join(rest),
        _ => root.join(typed),
    }
}

struct Picker {
    root: PathBuf,
    candidates: Vec<Candidate>,
    typed: String,
    selected: usize,
}

enum Step {
    Stay,
    Open(String),
    Quit,
}

impl Picker {
    fn key(&mut self, code: KeyCode, ctrl: bool) -> Step {
        let count = rank(&self.candidates, &self.typed).len();
        match code {
            KeyCode::Esc => return Step::Quit,
            KeyCode::Char('c') if ctrl => return Step::Quit,
            KeyCode::Enter => {
                let ranked = rank(&self.candidates, &self.typed);
                if let Some(target) = choice(&ranked, self.selected, &self.typed, &self.root) {
                    return Step::Open(target);
                }
            }
            KeyCode::Up | KeyCode::BackTab => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('p' | 'k') if ctrl => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('n' | 'j') if ctrl => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Backspace => {
                self.typed.pop();
                self.selected = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.typed.clear();
                self.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.typed.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        Step::Stay
    }

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let ranked = rank(&self.candidates, &self.typed);
        let dim = Style::default().fg(DIM);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Open  ", dim),
                Span::raw(self.typed.clone()),
                Span::styled("▏", dim),
            ]),
            Line::from(Span::styled(
                format!(
                    "{} of {} · a path or https:// link opens as typed · enter open · esc quit",
                    ranked.len(),
                    self.candidates.len()
                ),
                dim,
            )),
            Line::raw(""),
        ];
        let rows = area.height.saturating_sub(5) as usize;
        let first = self.selected.saturating_sub(rows.saturating_sub(1));
        if ranked.is_empty() {
            let hint = if self.candidates.is_empty() {
                "No PDF, EPUB or markdown files here. Type a path or a link."
            } else {
                "Nothing matches. Type a path or a link to open it as it is."
            };
            lines.push(Line::from(Span::styled(hint, dim)));
        }
        for (at, candidate) in ranked.iter().enumerate().skip(first).take(rows) {
            let picked = at == self.selected;
            let mark = if picked { "› " } else { "  " };
            let style = if picked {
                Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::raw(mark),
                Span::styled(candidate.shown.clone(), style),
            ];
            if let Some(percent) = candidate.percent {
                spans.push(Span::styled(format!("  {percent}%"), dim));
            }
            lines.push(Line::from(spans));
        }
        let body = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(1),
        };
        f.render_widget(Paragraph::new(lines), body);
    }
}

/// Shows the picker until a document is chosen, or `None` when it's closed.
pub fn pick(root: &Path, recent: Vec<(String, usize)>) -> io::Result<Option<String>> {
    let files = walk(root);
    let mut picker = Picker {
        root: root.to_path_buf(),
        candidates: merge(recent, root, files),
        typed: String::new(),
        selected: 0,
    };
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle("leaf"));
    let result = (|| loop {
        terminal.draw(|f| picker.draw(f))?;
        if !event::poll(Duration::from_secs(60))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind != KeyEventKind::Release
        {
            match picker.key(key.code, key.modifiers.contains(KeyModifiers::CONTROL)) {
                Step::Stay => {}
                Step::Open(target) => return Ok(Some(target)),
                Step::Quit => return Ok(None),
            }
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(shown: &str) -> Candidate {
        Candidate {
            target: format!("/r/{shown}"),
            shown: shown.to_owned(),
            percent: None,
        }
    }

    #[test]
    fn names_beat_folders_and_every_word_must_match() {
        let all = [
            candidate("rust/notes.md"),
            candidate("books/rust-book.pdf"),
            candidate("papers/attention.pdf"),
        ];
        let shown = |typed: &str| -> Vec<String> {
            rank(&all, typed).iter().map(|c| c.shown.clone()).collect()
        };
        assert_eq!(shown("rust"), ["books/rust-book.pdf", "rust/notes.md"]);
        assert_eq!(shown("rust pdf"), ["books/rust-book.pdf"]);
        assert_eq!(shown("atn"), ["papers/attention.pdf"]);
        assert_eq!(shown("").len(), 3);
    }

    #[test]
    fn a_typed_link_or_path_opens_as_it_is() {
        let dir = std::env::temp_dir().join(format!("leaf-picker-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("deep")).unwrap();
        std::fs::write(dir.join("deep/a.md"), "# A").unwrap();
        std::fs::write(dir.join("b.txt"), "no").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/c.md"), "hidden").unwrap();
        let files = walk(&dir);
        assert_eq!(
            files.iter().map(|c| c.shown.as_str()).collect::<Vec<_>>(),
            ["deep/a.md"]
        );
        let none: [&Candidate; 0] = [];
        assert_eq!(
            choice(&none, 0, "https://example.com/post", &dir).as_deref(),
            Some("https://example.com/post")
        );
        assert_eq!(
            choice(&none, 0, "b.txt", &dir),
            Some(dir.join("b.txt").display().to_string())
        );
        assert_eq!(choice(&none, 0, "missing.pdf", &dir), None);
        let recent = merge(
            vec![
                (dir.join("deep/a.md").display().to_string(), 30),
                ("/gone.pdf".into(), 9),
            ],
            &dir,
            files,
        );
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].percent, Some(30));
        let _ = std::fs::remove_dir_all(dir);
    }
}
