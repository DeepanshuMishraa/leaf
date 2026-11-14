//! A Margin review: the notes left on a plan or a diff, and the verdict they
//! go back to the agent with, rendered as one numbered message.

use std::fmt::Write as _;

use crate::comment::{Anchor, Side};

/// What the reviewer decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    ApproveWithNotes,
    RequestChanges,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Approve => "approved",
            Verdict::ApproveWithNotes => "approved with notes",
            Verdict::RequestChanges => "changes requested",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::ApproveWithNotes => "approve_with_notes",
            Verdict::RequestChanges => "request_changes",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Verdict::Approve),
            "approve_with_notes" => Some(Verdict::ApproveWithNotes),
            "request_changes" => Some(Verdict::RequestChanges),
            _ => None,
        }
    }

    fn sentence(self) -> &'static str {
        match self {
            Verdict::Approve => "Approved. Go ahead.",
            Verdict::ApproveWithNotes => "Approved with notes. Go ahead and apply these.",
            Verdict::RequestChanges => "Changes requested. Revise, then submit again.",
        }
    }

    fn closing(self, plan: bool) -> Option<&'static str> {
        match (self, plan) {
            (Verdict::Approve, _) => None,
            (Verdict::ApproveWithNotes, _) => Some("Apply every numbered item as you go."),
            (Verdict::RequestChanges, true) => {
                Some("Address every numbered item, then submit the revised plan for review again.")
            }
            (Verdict::RequestChanges, false) => {
                Some("Address every numbered item, then ask for a review again.")
            }
        }
    }
}

/// Where a plan note points: a whole block, or a passage selected in one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanTarget {
    Block {
        /// 1-based, as the gutter shows it.
        index: usize,
        heading: Vec<String>,
        /// Source lines, 1-based and inclusive.
        lines: (usize, usize),
        text: String,
    },
    Passage {
        quote: String,
        block: usize,
        lines: Option<(usize, usize)>,
    },
}

/// What a plan note asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEdit {
    Comment(String),
    Remove,
    Replace(String),
}

impl PlanEdit {
    pub fn kind(&self) -> &'static str {
        match self {
            PlanEdit::Comment(_) => "comment",
            PlanEdit::Remove => "remove",
            PlanEdit::Replace(_) => "replace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanNote {
    pub target: PlanTarget,
    pub edit: PlanEdit,
}

/// Where a code note points: a whole file, or lines in it with the hunk around them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeTarget {
    File { path: String },
    Lines { anchor: Anchor, hunk: String },
}

impl CodeTarget {
    fn path(&self) -> &str {
        match self {
            CodeTarget::File { path } => path,
            CodeTarget::Lines { anchor, .. } => &anchor.file,
        }
    }
}

/// A note on code is always a comment: a file can't be "removed" by a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeNote {
    pub target: CodeTarget,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notes {
    Plan(Vec<PlanNote>),
    /// In diff order: the caller sorts them by file, then line.
    Code(Vec<CodeNote>),
}

impl Notes {
    fn len(&self) -> usize {
        match self {
            Notes::Plan(notes) => notes.len(),
            Notes::Code(notes) => notes.len(),
        }
    }
}

/// How a review screen was left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A verdict was sent; `text` is the rendered review.
    Sent { verdict: Verdict, text: String },
    /// Closed without a verdict.
    Closed,
}

/// One round's review, ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// What was reviewed: a plan's path, or a diff's title.
    pub subject: String,
    /// The plan's round, from the second submission on.
    pub round: Option<u32>,
    pub global: Option<String>,
    pub notes: Notes,
}

/// A quoted block longer than this is cut, so the agent sees which block
/// without reading all of it again.
const QUOTE_LINES: usize = 6;

impl Review {
    pub fn has_notes(&self) -> bool {
        self.notes.len() > 0 || self.global.as_deref().is_some_and(|g| !g.trim().is_empty())
    }

    /// The verdict that goes out: an approval with notes says so, so notes
    /// are never sent under a bare "go ahead".
    pub fn verdict(&self, asked: Verdict) -> Verdict {
        match asked {
            Verdict::Approve if self.has_notes() => Verdict::ApproveWithNotes,
            Verdict::ApproveWithNotes if !self.has_notes() => Verdict::Approve,
            other => other,
        }
    }

    /// The message the agent receives.
    pub fn render(&self, asked: Verdict) -> String {
        let verdict = self.verdict(asked);
        let mut out = String::new();
        let round = self
            .round
            .filter(|round| *round > 1)
            .map(|round| format!(" (round {round})"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "# Review of {}{round}: {}\n",
            self.subject,
            verdict.label()
        );
        let _ = writeln!(out, "{}", verdict.sentence());
        if let Some(global) = self.global.as_deref().map(str::trim)
            && !global.is_empty()
        {
            let _ = writeln!(out, "\nGlobal: {global}");
        }
        let plan = matches!(self.notes, Notes::Plan(_));
        match &self.notes {
            Notes::Plan(notes) => render_plan(&mut out, notes),
            Notes::Code(notes) => render_code(&mut out, notes),
        }
        if self.notes.len() > 0
            && let Some(closing) = verdict.closing(plan)
        {
            let _ = writeln!(out, "\n{closing}");
        }
        out
    }
}

fn lines_label(lines: (usize, usize)) -> String {
    if lines.0 == lines.1 {
        format!("line {}", lines.0)
    } else {
        format!("lines {}-{}", lines.0, lines.1)
    }
}

fn quote(out: &mut String, text: &str, cap: Option<usize>) {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let shown = cap.map_or(lines.len(), |cap| cap.min(lines.len()));
    for line in &lines[..shown] {
        if line.trim().is_empty() {
            out.push_str(">\n");
        } else {
            let _ = writeln!(out, "> {line}");
        }
    }
    if shown < lines.len() {
        let _ = writeln!(out, "> …");
    }
}

fn render_plan(out: &mut String, notes: &[PlanNote]) {
    for (number, note) in notes.iter().enumerate() {
        let number = number + 1;
        let kind = note.edit.kind();
        out.push('\n');
        match &note.target {
            PlanTarget::Block {
                index,
                heading,
                lines,
                text,
            } => {
                let under = if heading.is_empty() {
                    String::new()
                } else {
                    format!(", \"{}\"", heading.join(" > "))
                };
                let _ = writeln!(
                    out,
                    "## {number}. Block {index}{under} ({}): {kind}",
                    lines_label(*lines)
                );
                quote(out, text, Some(QUOTE_LINES));
            }
            PlanTarget::Passage {
                quote: passage,
                block,
                lines,
            } => {
                let place = match lines {
                    Some(lines) => format!("Block {block}, {}", lines_label(*lines)),
                    None => format!("Block {block}"),
                };
                let _ = writeln!(out, "## {number}. {place}: {kind}");
                quote(out, passage, None);
            }
        }
        match &note.edit {
            PlanEdit::Comment(body) => {
                let _ = writeln!(out, "\n{}", body.trim());
            }
            PlanEdit::Remove => {
                let _ = writeln!(out, "\nRemove this from the plan.");
            }
            PlanEdit::Replace(with) => {
                let _ = writeln!(out, "\nReplace it with:\n");
                quote(out, with, None);
            }
        }
    }
}

fn render_code(out: &mut String, notes: &[CodeNote]) {
    // Grouped by file, files in the order they first appear.
    let mut files: Vec<&str> = Vec::new();
    for note in notes {
        let path = note.target.path();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    let mut number = 0;
    for file in files {
        let _ = writeln!(out, "\n## {file}");
        // The whole-file notes first, then the line notes in order.
        let in_file = notes.iter().filter(|note| note.target.path() == file);
        let (whole, lines): (Vec<&CodeNote>, Vec<&CodeNote>) =
            in_file.partition(|note| matches!(note.target, CodeTarget::File { .. }));
        for note in whole.into_iter().chain(lines) {
            number += 1;
            match &note.target {
                CodeTarget::File { .. } => {
                    let _ = writeln!(out, "\n### {number}. Whole file\n\n{}", note.body.trim());
                }
                CodeTarget::Lines { anchor, hunk } => {
                    let side = match anchor.side {
                        Side::New => "new side",
                        Side::Old => "old side",
                    };
                    let range = if anchor.start_line == anchor.end_line {
                        format!("Line {}", anchor.start_line)
                    } else {
                        format!("Lines {}-{}", anchor.start_line, anchor.end_line)
                    };
                    let hunk = hunk.trim_end();
                    if hunk.is_empty() {
                        // The anchored lines left the diff, so there's nothing
                        // to quote; the note still goes.
                        let _ = writeln!(
                            out,
                            "\n### {number}. {range} ({side})\n\n{}",
                            note.body.trim()
                        );
                    } else {
                        let _ = writeln!(
                            out,
                            "\n### {number}. {range} ({side})\n\n````diff\n{hunk}\n````\n\n{}",
                            note.body.trim()
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(notes: Vec<PlanNote>, global: Option<&str>) -> Review {
        Review {
            subject: "docs/plans/auth.md".into(),
            round: None,
            global: global.map(str::to_owned),
            notes: Notes::Plan(notes),
        }
    }

    fn block(index: usize, text: &str) -> PlanTarget {
        PlanTarget::Block {
            index,
            heading: vec!["Phase 2".into(), "Tokens".into()],
            lines: (12, 13),
            text: text.into(),
        }
    }

    #[test]
    fn a_bare_approval_is_one_line_with_nothing_to_address() {
        let review = plan(Vec::new(), None);
        assert_eq!(
            review.render(Verdict::Approve),
            "# Review of docs/plans/auth.md: approved\n\nApproved. Go ahead.\n"
        );
    }

    #[test]
    fn approving_with_notes_says_so_and_approving_without_them_does_not() {
        let with = plan(Vec::new(), Some("Ship it, but log the migration."));
        assert_eq!(with.verdict(Verdict::Approve), Verdict::ApproveWithNotes);
        let without = plan(Vec::new(), Some("   "));
        assert_eq!(without.verdict(Verdict::ApproveWithNotes), Verdict::Approve);
        assert_eq!(
            without.verdict(Verdict::RequestChanges),
            Verdict::RequestChanges
        );
    }

    #[test]
    fn plan_notes_are_numbered_with_their_block_quote_and_edit() {
        let review = Review {
            round: Some(2),
            ..plan(
                vec![
                    PlanNote {
                        target: block(4, "## Tokens\nRotate the token on every privilege change"),
                        edit: PlanEdit::Comment("That logs people out.".into()),
                    },
                    PlanNote {
                        target: PlanTarget::Block {
                            index: 9,
                            heading: Vec::new(),
                            lines: (30, 30),
                            text: "Rewrite the session store".into(),
                        },
                        edit: PlanEdit::Remove,
                    },
                    PlanNote {
                        target: PlanTarget::Passage {
                            quote: "use bcrypt".into(),
                            block: 11,
                            lines: Some((41, 41)),
                        },
                        edit: PlanEdit::Replace("use argon2id".into()),
                    },
                ],
                Some("Split the migration out."),
            )
        };
        assert_eq!(
            review.render(Verdict::RequestChanges),
            "# Review of docs/plans/auth.md (round 2): changes requested

Changes requested. Revise, then submit again.

Global: Split the migration out.

## 1. Block 4, \"Phase 2 > Tokens\" (lines 12-13): comment
> ## Tokens
> Rotate the token on every privilege change

That logs people out.

## 2. Block 9 (line 30): remove
> Rewrite the session store

Remove this from the plan.

## 3. Block 11, line 41: replace
> use bcrypt

Replace it with:

> use argon2id

Address every numbered item, then submit the revised plan for review again.
"
        );
    }

    #[test]
    fn a_long_block_is_quoted_only_in_part() {
        let text = (1..=10)
            .map(|n| format!("step {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let review = plan(
            vec![PlanNote {
                target: block(1, &text),
                edit: PlanEdit::Comment("Too many steps.".into()),
            }],
            None,
        );
        let rendered = review.render(Verdict::RequestChanges);
        assert!(rendered.contains("> step 6\n> …\n"));
        assert!(!rendered.contains("step 7"));
    }

    #[test]
    fn code_notes_are_grouped_by_file_with_file_notes_first() {
        let lines = |file: &str, line: u32, body: &str| CodeNote {
            target: CodeTarget::Lines {
                anchor: Anchor {
                    file: file.into(),
                    side: Side::New,
                    start_line: line,
                    end_line: line,
                },
                hunk: format!("+line {line}"),
            },
            body: body.into(),
        };
        let review = Review {
            subject: "feat/x against main".into(),
            round: None,
            global: None,
            notes: Notes::Code(vec![
                lines("src/a.rs", 4, "Name this."),
                lines("src/b.rs", 9, "Leaks."),
                CodeNote {
                    target: CodeTarget::File {
                        path: "src/a.rs".into(),
                    },
                    body: "Split this file.".into(),
                },
            ]),
        };
        assert_eq!(
            review.render(Verdict::RequestChanges),
            "# Review of feat/x against main: changes requested

Changes requested. Revise, then submit again.

## src/a.rs

### 1. Whole file

Split this file.

### 2. Line 4 (new side)

````diff
+line 4
````

Name this.

## src/b.rs

### 3. Line 9 (new side)

````diff
+line 9
````

Leaks.

Address every numbered item, then ask for a review again.
"
        );
    }

    #[test]
    fn verdicts_round_trip_through_their_names() {
        for verdict in [
            Verdict::Approve,
            Verdict::ApproveWithNotes,
            Verdict::RequestChanges,
        ] {
            assert_eq!(Verdict::parse(verdict.as_str()), Some(verdict));
        }
        assert_eq!(Verdict::parse("maybe"), None);
    }
}
