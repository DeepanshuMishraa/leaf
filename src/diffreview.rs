//! Margin's notes on a diff beyond its line comments: one note per file, a
//! global note, and which files have been looked at. Builds the one review
//! that goes back to the agent.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use crate::comment::State;
use crate::diff::Diff;
use crate::progress::SavedComment;
use crate::review::{CodeNote, CodeTarget, Notes, Review};

/// Hears the file and global notes and viewed marks, as JSON, each time they
/// change; `None` once the review is sent.
pub type SaveNotes = Box<dyn FnMut(Option<&str>)>;

/// The draft and saver for another diff key, for when the screen switches to
/// a different diff (`o` to the paired view): its notes are its own.
pub type Reopen = Box<dyn FnMut(&str) -> (Option<String>, SaveNotes)>;

/// Where `S` sends the review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sends {
    /// Into the agent pane now, staying in the diff (`leaf diff`).
    Agent,
    /// Back to the caller, closing the screen (`leaf review`).
    Return,
}

/// How a diff screen keeps and sends its review.
pub struct Margin {
    /// Notes left last time and not sent, as `save` wrote them.
    pub draft: Option<String>,
    pub save: SaveNotes,
    pub sends: Sends,
    /// Open on the overview of every file rather than the first file.
    pub overview_first: bool,
    /// Loads another key's notes; without it, notes stay with the first key's
    /// draft for the whole run.
    pub reopen: Option<Reopen>,
}

impl Default for Margin {
    fn default() -> Self {
        Margin {
            draft: None,
            save: Box::new(|_| {}),
            sends: Sends::Agent,
            overview_first: false,
            reopen: None,
        }
    }
}

/// The notes on a diff that aren't line comments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNotes {
    pub global: Option<String>,
    /// One note per file, by path.
    pub files: BTreeMap<String, String>,
    /// Files marked viewed, with the fingerprint of what was viewed.
    pub viewed: HashMap<String, u64>,
}

impl FileNotes {
    pub fn count(&self) -> usize {
        self.files.len() + usize::from(self.global.is_some())
    }

    /// Moves a note kept under a renamed file's old path to its new one, so
    /// editing and deleting it by the shown path reach it. `true` when any
    /// moved.
    pub fn follow_renames(&mut self, diff: &Diff) -> bool {
        let mut moved = false;
        for file in diff.files() {
            let (Some(old), Some(new)) = (&file.old_path, &file.new_path) else {
                continue;
            };
            if old != new
                && !self.files.contains_key(new)
                && let Some(note) = self.files.remove(old)
            {
                self.files.insert(new.clone(), note);
                moved = true;
            }
        }
        moved
    }

    pub fn set_global(&mut self, text: &str) {
        let text = text.trim();
        self.global = (!text.is_empty()).then(|| text.to_owned());
    }

    pub fn set_file(&mut self, path: &str, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            self.files.remove(path);
        } else {
            self.files.insert(path.to_owned(), text.to_owned());
        }
    }

    pub fn is_viewed(&self, diff: &Diff, file: usize) -> bool {
        let Some(path) = diff.file(file).map(|f| f.path()) else {
            return false;
        };
        self.viewed.get(path) == Some(&fingerprint(diff, file))
    }

    /// Marks or unmarks file `file` as viewed; returns whether it now is.
    pub fn toggle_viewed(&mut self, diff: &Diff, file: usize) -> bool {
        let Some(path) = diff.file(file).map(|f| f.path().to_owned()) else {
            return false;
        };
        if self.is_viewed(diff, file) {
            self.viewed.remove(&path);
            false
        } else {
            self.viewed.insert(path, fingerprint(diff, file));
            true
        }
    }

    /// Clears what the review sent: the notes, not the viewed marks.
    pub fn sent(&mut self) {
        self.global = None;
        self.files.clear();
    }

    pub fn to_json(&self) -> Option<String> {
        if self.count() == 0 && self.viewed.is_empty() {
            return None;
        }
        let viewed: BTreeMap<&String, String> = self
            .viewed
            .iter()
            .map(|(path, print)| (path, print.to_string()))
            .collect();
        Some(
            serde_json::json!({
                "global": self.global,
                "files": self.files,
                "viewed": viewed,
            })
            .to_string(),
        )
    }

    /// Notes saved earlier. Notes on files no longer in the diff stay, so a
    /// reload that drops a file and brings it back keeps them; viewed marks
    /// stay too, and only count while the file is unchanged.
    pub fn from_json(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return FileNotes::default();
        };
        let global = value["global"]
            .as_str()
            .map(str::to_owned)
            .filter(|g| !g.trim().is_empty());
        let files = value["files"]
            .as_object()
            .map(|files| {
                files
                    .iter()
                    .filter_map(|(path, note)| Some((path.clone(), note.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        let viewed = value["viewed"]
            .as_object()
            .map(|viewed| {
                viewed
                    .iter()
                    .filter_map(|(path, print)| {
                        Some((path.clone(), print.as_str()?.parse::<u64>().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        FileNotes {
            global,
            files,
            viewed,
        }
    }
}

/// FNV-1a as a `Hasher`. Fingerprints are saved with viewed marks, so they
/// must come out the same after a rebuild, which `DefaultHasher` doesn't
/// promise.
struct Fnv(u64);

impl Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        self.0 = crate::plugin::fnv1a_continue(self.0, bytes);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Stands for file `file`'s changes: it differs once any of them does.
pub fn fingerprint(diff: &Diff, file: usize) -> u64 {
    let mut hasher = Fnv(crate::plugin::fnv1a_bytes(&[]));
    let Some(info) = diff.file(file) else {
        return 0;
    };
    // Both paths and the change's metadata, so a rename, mode change or
    // binary switch lapses an earlier viewed mark even with the same rows.
    info.old_path.hash(&mut hasher);
    info.new_path.hash(&mut hasher);
    info.binary.hash(&mut hasher);
    info.note.hash(&mut hasher);
    info.identity.hash(&mut hasher);
    for hunk in &info.hunks {
        for row in hunk.first.saturating_sub(1)..hunk.end {
            diff.row_text(row).hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// How many lines around a commented range the agent sees.
const HUNK_PAD: usize = 8;

/// The review of `diff`: the draft line comments, the file notes and the
/// global note, in diff order. Line comments whose lines left the diff are
/// still sent, without their hunk.
pub fn build(diff: &Diff, subject: &str, notes: &FileNotes, comments: &[SavedComment]) -> Review {
    let order = |path: &str| {
        diff.files()
            .iter()
            .position(|file| {
                file.new_path.as_deref() == Some(path) || file.old_path.as_deref() == Some(path)
            })
            .unwrap_or(usize::MAX)
    };
    let mut code: Vec<(usize, u32, CodeNote)> = Vec::new();
    for (path, body) in &notes.files {
        code.push((
            order(path),
            0,
            CodeNote {
                target: CodeTarget::File { path: path.clone() },
                body: body.clone(),
            },
        ));
    }
    for saved in comments
        .iter()
        .filter(|saved| saved.comment.state == State::Draft)
    {
        let anchor = &saved.comment.anchor;
        let first = diff.exact_row(&anchor.file, anchor.side, anchor.start_line);
        let last = diff
            .exact_row(&anchor.file, anchor.side, anchor.end_line)
            .or(first);
        let hunk = match (first, last) {
            (Some(first), Some(last)) => diff.neighborhood(first, last, HUNK_PAD),
            _ => String::new(),
        };
        code.push((
            order(&anchor.file),
            anchor.start_line,
            CodeNote {
                target: CodeTarget::Lines {
                    anchor: anchor.clone(),
                    hunk,
                },
                body: saved.comment.body.clone(),
            },
        ));
    }
    code.sort_by_key(|(file, line, _)| (*file, *line));
    Review {
        subject: subject.to_owned(),
        round: None,
        global: notes.global.clone(),
        notes: Notes::Code(code.into_iter().map(|(_, _, note)| note).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::{Anchor, Comment, Side};
    use crate::review::Verdict;

    const PATCH: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {}
+fn b() {}
 fn c() {}
diff --git a/src/z.rs b/src/z.rs
--- a/src/z.rs
+++ b/src/z.rs
@@ -1 +1 @@
-old
+new
";

    #[test]
    fn a_new_revision_of_a_binary_or_mode_change_has_a_new_fingerprint() {
        let binary = |index: &str| {
            Diff::parse(format!(
                "diff --git a/logo.png b/logo.png\nindex {index} 100644\nBinary files a/logo.png and b/logo.png differ\n"
            ))
        };
        assert_ne!(
            fingerprint(&binary("1111111..2222222"), 0),
            fingerprint(&binary("1111111..3333333"), 0)
        );
        let mode = |new: &str| {
            Diff::parse(format!(
                "diff --git a/run.sh b/run.sh\nold mode 100644\nnew mode {new}\n"
            ))
        };
        assert_ne!(
            fingerprint(&mode("100755"), 0),
            fingerprint(&mode("100775"), 0)
        );
    }

    fn comment(file: &str, line: u32, body: &str, state: State) -> SavedComment {
        SavedComment {
            id: 1,
            comment: Comment {
                anchor: Anchor {
                    file: file.into(),
                    side: Side::New,
                    start_line: line,
                    end_line: line,
                },
                body: body.into(),
                state,
            },
        }
    }

    #[test]
    fn the_review_has_draft_comments_and_notes_in_diff_order() {
        let diff = Diff::parse(PATCH.into());
        let mut notes = FileNotes::default();
        notes.set_global("Looks close.");
        notes.set_file("src/z.rs", "Why rename?");
        notes.set_file("src/a.rs", "  ");
        let comments = [
            comment("src/z.rs", 1, "new what?", State::Draft),
            comment("src/a.rs", 2, "name b better", State::Draft),
            comment("src/a.rs", 1, "already sent", State::Agent),
        ];
        let review = build(&diff, "feat against main", &notes, &comments);
        let text = review.render(Verdict::RequestChanges);
        let a = text.find("## src/a.rs").unwrap_or(usize::MAX);
        let z = text.find("## src/z.rs").unwrap_or(0);
        assert!(a < z, "{text}");
        assert!(text.contains("Global: Looks close."));
        assert!(text.contains("### 1. Line 2 (new side)\n\n````diff\n@@ -1,2 +1,3 @@\n fn a() {}\n+fn b() {}\n fn c() {}\n````\n\nname b better"), "{text}");
        assert!(
            text.contains("### 2. Whole file\n\nWhy rename?\n\n### 3. Line 1"),
            "{text}"
        );
        assert!(!text.contains("already sent"));
    }

    #[test]
    fn a_viewed_mark_lapses_when_the_file_changes() {
        let diff = Diff::parse(PATCH.into());
        let mut notes = FileNotes::default();
        assert!(notes.toggle_viewed(&diff, 0));
        assert!(notes.is_viewed(&diff, 0));
        assert!(!notes.is_viewed(&diff, 1));
        let changed = Diff::parse(PATCH.replace("+fn b() {}", "+fn b2() {}"));
        assert!(!notes.is_viewed(&changed, 0));
        assert!(notes.is_viewed(&Diff::parse(PATCH.into()), 0));
        assert!(!notes.toggle_viewed(&diff, 0));
        assert!(!notes.is_viewed(&diff, 0));
    }

    #[test]
    fn notes_round_trip_and_sending_keeps_only_viewed_marks() {
        let diff = Diff::parse(PATCH.into());
        let mut notes = FileNotes::default();
        assert_eq!(notes.to_json(), None);
        notes.set_global("g");
        notes.set_file("src/a.rs", "f");
        notes.toggle_viewed(&diff, 1);
        let json = notes.to_json().unwrap_or_default();
        assert_eq!(FileNotes::from_json(&json), notes);
        notes.sent();
        assert_eq!(notes.count(), 0);
        assert!(notes.is_viewed(&diff, 1));
        assert_eq!(FileNotes::from_json("not json"), FileNotes::default());
    }
}
