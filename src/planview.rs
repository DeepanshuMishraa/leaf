//! Margin's plan screen: the plan split into numbered blocks, notes on each
//! block or on the whole plan, and a verdict that sends them all at once.

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::SetTitle;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as Frame_, BorderType, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::hue::Hue;
use crate::planblocks::{self, Block};
use crate::render::{self, DIM, Laid};
use crate::review::{Notes, Outcome, PlanEdit, PlanNote, PlanTarget, Review, Verdict};

/// Widest the plan's text gets.
const MAX_TEXT_WIDTH: u16 = 80;
/// Block numbers and the bar marking the block under the cursor.
const GUTTER: u16 = 6;
/// How many blocks ctrl+d and ctrl+u move.
const HALF_STEP: usize = 5;
const WHEEL_STEP: usize = 3;
const ACCENT: Color = Color::Blue;
const SELECTION: Color = Color::Indexed(8);

/// Hears the unsent notes, as JSON, each time they change; `None` once the
/// review is sent and there's nothing left to keep.
pub type SaveDraft = Box<dyn FnMut(Option<&str>)>;

/// What is being reviewed.
pub struct Plan {
    /// Shown in the header and the review: the plan's path, usually.
    pub subject: String,
    pub source: String,
    /// The plan's round, from the second submission on.
    pub round: Option<u32>,
    /// Notes left last time and not sent, as `save` wrote them.
    pub draft: Option<String>,
    pub save: SaveDraft,
}

/// Where the cursor is: the row for notes on the whole plan, or a block
/// (0-based into the blocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Global,
    Block(usize),
}

/// What the composer is writing. `editing` names the note being rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Writing {
    Comment {
        target: PlanTarget,
        editing: Option<usize>,
    },
    Replace {
        target: PlanTarget,
        editing: Option<usize>,
    },
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Composer {
    writing: Writing,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    None,
    Help,
    Verdict,
    Quit,
    /// Every note, with the one picked.
    Notes {
        selected: usize,
    },
}

/// Text selected with the mouse inside one block: character offsets into
/// that block's laid-out text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    block: usize,
    start: usize,
    end: usize,
}

/// One screen row of the plan, and what it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    GlobalTitle,
    GlobalNote,
    Text { block: usize, row: usize },
    Note { block: usize, note: usize },
    Gap,
}

impl Row {
    fn focus(self) -> Option<Focus> {
        match self {
            Row::GlobalTitle | Row::GlobalNote => Some(Focus::Global),
            Row::Text { block, .. } | Row::Note { block, .. } => Some(Focus::Block(block)),
            Row::Gap => None,
        }
    }
}

fn hue_of(edit: &PlanEdit) -> Color {
    match edit {
        PlanEdit::Comment(_) => Hue::Yellow.marker(),
        PlanEdit::Remove => Hue::Red.marker(),
        PlanEdit::Replace(_) => Hue::Blue.marker(),
    }
}

fn no_images(_: &str) -> Option<(u32, u32)> {
    None
}

struct App {
    subject: String,
    round: Option<u32>,
    blocks: Vec<Block>,
    /// Each block laid out at `laid_width`.
    laid: Vec<Laid>,
    laid_width: u16,
    notes: Vec<PlanNote>,
    global: Option<String>,
    focus: Focus,
    scroll: usize,
    /// Rows of plan on screen, as last drawn.
    view_height: usize,
    /// The plan's rows on screen, as last drawn, for clicks.
    shown: Vec<(u16, Row)>,
    composer: Option<Composer>,
    dialog: Dialog,
    toast: Option<String>,
    outcome: Option<Outcome>,
    selection: Option<Selection>,
    /// Where a mouse drag started: a block and an offset in its text.
    drag: Option<(usize, usize)>,
    /// The text column's left edge, as last drawn, for clicks.
    text_x: u16,
    save: SaveDraft,
}

impl App {
    fn new(plan: Plan) -> Self {
        let blocks = planblocks::blocks(&plan.source);
        let focus = if blocks.is_empty() {
            Focus::Global
        } else {
            Focus::Block(0)
        };
        let mut app = App {
            subject: plan.subject,
            round: plan.round,
            blocks,
            laid: Vec::new(),
            laid_width: 0,
            notes: Vec::new(),
            global: None,
            focus,
            scroll: 0,
            view_height: 1,
            shown: Vec::new(),
            composer: None,
            dialog: Dialog::None,
            toast: None,
            outcome: None,
            selection: None,
            drag: None,
            text_x: 0,
            save: plan.save,
        };
        if let Some(draft) = plan.draft.as_deref() {
            app.restore(draft);
        }
        app
    }

    // Unsent notes, kept between runs. A note is tied to its block's text,
    // so one whose block was rewritten since is dropped rather than misplaced.

    fn draft_json(&self) -> Option<String> {
        if self.note_count() == 0 {
            return None;
        }
        let notes: Vec<serde_json::Value> = self
            .notes
            .iter()
            .map(|note| {
                let block = block_of(&note.target);
                let quote = match &note.target {
                    PlanTarget::Passage { quote, .. } => Some(quote.as_str()),
                    PlanTarget::Block { .. } => None,
                };
                let body = match &note.edit {
                    PlanEdit::Comment(body) | PlanEdit::Replace(body) => Some(body.as_str()),
                    PlanEdit::Remove => None,
                };
                serde_json::json!({
                    "block": self.blocks.get(block).map(|b| b.text.as_str()),
                    "index": block,
                    "kind": note.edit.kind(),
                    "quote": quote,
                    "body": body,
                })
            })
            .collect();
        Some(serde_json::json!({ "global": self.global, "notes": notes }).to_string())
    }

    fn restore(&mut self, draft: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(draft) else {
            return;
        };
        self.global = value["global"]
            .as_str()
            .map(str::to_owned)
            .filter(|g| !g.is_empty());
        let mut dropped = 0;
        for saved in value["notes"].as_array().into_iter().flatten() {
            let text = saved["block"].as_str().unwrap_or_default();
            let index = saved["index"].as_u64().map(|i| i as usize);
            // The same place first, then the same text anywhere.
            let block = index
                .filter(|i| self.blocks.get(*i).is_some_and(|b| b.text == text))
                .or_else(|| self.blocks.iter().position(|b| b.text == text));
            let body = saved["body"].as_str().map(str::to_owned);
            let edit = match (saved["kind"].as_str(), body) {
                (Some("comment"), Some(body)) => PlanEdit::Comment(body),
                (Some("replace"), Some(body)) => PlanEdit::Replace(body),
                (Some("remove"), _) => PlanEdit::Remove,
                _ => {
                    dropped += 1;
                    continue;
                }
            };
            let Some(block) = block else {
                dropped += 1;
                continue;
            };
            let target = match saved["quote"].as_str() {
                Some(quote) => self.passage_target(block, quote),
                None => self.block_target(block),
            };
            self.notes.push(PlanNote { target, edit });
        }
        let kept = self.note_count();
        self.toast = Some(match (kept, dropped) {
            (0, 0) => return,
            (kept, 0) => format!("{} from last time", plural(kept, "note")),
            (kept, dropped) => format!(
                "{} from last time; {} no longer match the plan and were dropped",
                plural(kept, "note"),
                dropped
            ),
        });
    }

    fn persist(&mut self) {
        let json = self.draft_json();
        (self.save)(json.as_deref());
    }

    fn lay_out(&mut self, width: u16) {
        if width == self.laid_width && self.laid.len() == self.blocks.len() {
            return;
        }
        self.laid_width = width;
        self.laid = self
            .blocks
            .iter()
            .map(|block| {
                // Layout spaces blocks apart; here the gaps are drawn between
                // blocks instead, so each starts on its first line of text.
                let mut laid = render::layout(&block.text, width, (8, 16), &no_images);
                let lead = laid.rows.iter().take_while(|r| r.segs.is_empty()).count();
                laid.rows.drain(..lead);
                while laid.rows.last().is_some_and(|r| r.segs.is_empty()) {
                    laid.rows.pop();
                }
                laid
            })
            .collect();
    }

    fn note_count(&self) -> usize {
        self.notes.len() + usize::from(self.global.is_some())
    }

    fn notes_on(&self, block: usize) -> impl Iterator<Item = (usize, &PlanNote)> {
        self.notes
            .iter()
            .enumerate()
            .filter(move |(_, note)| block_of(&note.target) == block)
    }

    /// Whether the whole block is marked for removal.
    fn removed(&self, block: usize) -> bool {
        self.notes_on(block).any(|(_, note)| {
            note.edit == PlanEdit::Remove && matches!(note.target, PlanTarget::Block { .. })
        })
    }

    /// Every row of the plan, top to bottom.
    fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row::GlobalTitle];
        if self.global.is_some() {
            rows.push(Row::GlobalNote);
        }
        rows.push(Row::Gap);
        for (block, laid) in self.laid.iter().enumerate() {
            for row in 0..laid.rows.len().max(1) {
                rows.push(Row::Text { block, row });
            }
            for (note, _) in self.notes_on(block) {
                rows.push(Row::Note { block, note });
            }
            rows.push(Row::Gap);
        }
        rows
    }

    fn focus_rows(&self, rows: &[Row]) -> (usize, usize) {
        let first = rows.iter().position(|row| row.focus() == Some(self.focus));
        let last = rows.iter().rposition(|row| row.focus() == Some(self.focus));
        (first.unwrap_or(0), last.unwrap_or(0))
    }

    /// Scrolls so the focused block is on screen, its top first.
    fn follow(&mut self) {
        let rows = self.rows();
        let (first, last) = self.focus_rows(&rows);
        let height = self.view_height.max(1);
        if first < self.scroll {
            self.scroll = first;
        } else if last >= self.scroll + height {
            self.scroll = (last + 1).saturating_sub(height).min(first);
        }
    }

    fn focus_index(&self) -> isize {
        match self.focus {
            Focus::Global => -1,
            Focus::Block(block) => block as isize,
        }
    }

    fn set_focus_index(&mut self, index: isize) {
        let last = self.blocks.len() as isize - 1;
        let index = index.clamp(-1, last);
        self.focus = if index < 0 {
            Focus::Global
        } else {
            Focus::Block(index as usize)
        };
        self.follow();
    }

    fn step(&mut self, by: isize) {
        self.set_focus_index(self.focus_index() + by);
    }

    /// The next (or previous) block with notes.
    fn step_noted(&mut self, forward: bool) {
        let here = self.focus_index();
        let noted = |block: usize| self.notes_on(block).next().is_some();
        let found = if forward {
            (0..self.blocks.len()).find(|b| *b as isize > here && noted(*b))
        } else {
            (0..self.blocks.len())
                .rev()
                .find(|b| (*b as isize) < here && noted(*b))
        };
        match found {
            Some(block) => self.set_focus_index(block as isize),
            None => self.toast = Some("no more blocks with notes that way".into()),
        }
    }

    fn scroll_by(&mut self, rows: isize) {
        let total = self.rows().len();
        let max = total.saturating_sub(self.view_height.max(1));
        self.scroll = self.scroll.saturating_add_signed(rows).min(max);
        // The cursor goes to the first block that starts on screen.
        let rows = self.rows();
        if let Some(focus) = rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .find(|(at, row)| {
                matches!(row, Row::Text { row: 0, .. } | Row::GlobalTitle) && *at >= self.scroll
            })
            .and_then(|(_, row)| row.focus())
        {
            self.focus = focus;
        }
    }

    /// The selection, or else the block under the cursor, as a note target.
    fn target_here(&self) -> Option<PlanTarget> {
        if let Some(selection) = self.selection {
            let quote = self.selected_text(selection);
            if !quote.trim().is_empty() {
                return Some(self.passage_target(selection.block, &quote));
            }
        }
        match self.focus {
            Focus::Block(block) => Some(self.block_target(block)),
            Focus::Global => None,
        }
    }

    fn compose_comment(&mut self) {
        let Some(target) = self.target_here() else {
            return self.compose_global();
        };
        self.selection = None;
        self.composer = Some(Composer {
            writing: Writing::Comment {
                target,
                editing: None,
            },
            text: String::new(),
        });
    }

    fn compose_replace(&mut self) {
        let Some(target) = self.target_here() else {
            self.toast = Some("move to a block, or select a passage, to replace it".into());
            return;
        };
        self.selection = None;
        // An earlier replacement of the same thing is edited, not doubled.
        let editing = self
            .notes
            .iter()
            .position(|note| note.target == target && matches!(note.edit, PlanEdit::Replace(_)));
        let text = match editing.map(|at| &self.notes[at].edit) {
            Some(PlanEdit::Replace(with)) => with.clone(),
            _ => target_text(&target).to_owned(),
        };
        self.composer = Some(Composer {
            writing: Writing::Replace { target, editing },
            text,
        });
    }

    fn compose_global(&mut self) {
        self.composer = Some(Composer {
            writing: Writing::Global,
            text: self.global.clone().unwrap_or_default(),
        });
    }

    /// Opens note `index` in the composer to rewrite it.
    fn edit_note(&mut self, index: usize) {
        let Some(note) = self.notes.get(index) else {
            return;
        };
        let target = note.target.clone();
        let (writing, text) = match &note.edit {
            PlanEdit::Comment(body) => (
                Writing::Comment {
                    target,
                    editing: Some(index),
                },
                body.clone(),
            ),
            PlanEdit::Replace(with) => (
                Writing::Replace {
                    target,
                    editing: Some(index),
                },
                with.clone(),
            ),
            PlanEdit::Remove => {
                self.toast = Some("a removal has no text to edit; x deletes it".into());
                return;
            }
        };
        self.dialog = Dialog::None;
        self.composer = Some(Composer { writing, text });
    }

    fn block_target(&self, block: usize) -> PlanTarget {
        let b = &self.blocks[block];
        PlanTarget::Block {
            index: b.index,
            heading: b.heading.clone(),
            lines: b.lines,
            text: b.text.clone(),
        }
    }

    /// A passage of block `block`, with the source lines it spans when found.
    fn passage_target(&self, block: usize, quote: &str) -> PlanTarget {
        let b = &self.blocks[block];
        let lines = planblocks::locate(&b.text, quote)
            .map(|(first, last)| (first + b.lines.0 - 1, last + b.lines.0 - 1));
        PlanTarget::Passage {
            quote: quote.trim().to_owned(),
            block: b.index,
            lines,
        }
    }

    fn selected_text(&self, selection: Selection) -> String {
        self.laid
            .get(selection.block)
            .map(|laid| {
                laid.text
                    .iter()
                    .skip(selection.start)
                    .take(selection.end.saturating_sub(selection.start))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_composer(&mut self) {
        let Some(composer) = self.composer.take() else {
            return;
        };
        let text = composer.text.trim().to_owned();
        match composer.writing {
            Writing::Global => {
                self.global = (!text.is_empty()).then_some(text);
                self.toast = Some(if self.global.is_some() {
                    "global note saved".into()
                } else {
                    "global note cleared".into()
                });
            }
            Writing::Comment { editing, .. } if text.is_empty() => {
                if let Some(at) = editing {
                    self.notes.remove(at);
                    self.toast = Some("comment deleted".into());
                } else {
                    self.toast = Some("empty comment discarded".into());
                }
            }
            Writing::Comment {
                editing: Some(at), ..
            } => {
                if let Some(note) = self.notes.get_mut(at) {
                    note.edit = PlanEdit::Comment(text);
                }
                self.toast = Some("comment updated".into());
            }
            Writing::Comment {
                target,
                editing: None,
            } => {
                self.toast = Some(format!("comment on {} saved", target_label(&target)));
                self.notes.push(PlanNote {
                    target,
                    edit: PlanEdit::Comment(text),
                });
            }
            Writing::Replace { target, editing } => {
                if let Some(at) = editing {
                    self.notes.remove(at);
                }
                if text.is_empty() || text == target_text(&target).trim() {
                    self.toast = Some("unchanged, so nothing to replace".into());
                } else {
                    // A replaced block isn't also removed.
                    self.notes
                        .retain(|note| !(note.target == target && note.edit == PlanEdit::Remove));
                    self.toast = Some(format!("replacement for {} saved", target_label(&target)));
                    self.notes.push(PlanNote {
                        target,
                        edit: PlanEdit::Replace(text),
                    });
                }
            }
        }
        self.persist();
        self.follow();
    }

    /// `x`: removes the selected passage, or marks (and unmarks) the block.
    fn remove_here(&mut self) {
        if let Some(selection) = self.selection.take() {
            let quote = self.selected_text(selection);
            if !quote.trim().is_empty() {
                let target = self.passage_target(selection.block, &quote);
                self.toast = Some(format!("{} marked for removal", target_label(&target)));
                self.notes.push(PlanNote {
                    target,
                    edit: PlanEdit::Remove,
                });
                self.persist();
                return;
            }
        }
        match self.focus {
            Focus::Block(block) => self.toggle_remove(block),
            Focus::Global => self.toast = Some("move to a block to mark it for removal".into()),
        }
    }

    fn delete_note(&mut self, index: usize) {
        if index < self.notes.len() {
            self.notes.remove(index);
            self.toast = Some("note deleted".into());
            self.persist();
        }
    }

    /// Note indexes in the order the list shows them: by block, then as written.
    fn listed(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.notes.len()).collect();
        order.sort_by_key(|at| block_of(&self.notes[*at].target));
        order
    }

    fn toggle_remove(&mut self, block: usize) {
        let target = self.block_target(block);
        if self.removed(block) {
            self.notes
                .retain(|note| !(note.target == target && note.edit == PlanEdit::Remove));
            self.toast = Some(format!("block {} kept", block + 1));
        } else {
            // A removed block isn't also replaced.
            self.notes.retain(|note| {
                !(note.target == target && matches!(note.edit, PlanEdit::Replace(_)))
            });
            self.notes.push(PlanNote {
                target,
                edit: PlanEdit::Remove,
            });
            self.toast = Some(format!("block {} marked for removal", block + 1));
        }
        self.persist();
    }

    fn clear_block(&mut self, block: usize) {
        let before = self.notes.len();
        self.notes.retain(|note| block_of(&note.target) != block);
        let cleared = before - self.notes.len();
        self.persist();
        self.toast = Some(match cleared {
            0 => "no notes on this block".into(),
            1 => "1 note cleared".into(),
            n => format!("{n} notes cleared"),
        });
    }

    fn review(&self) -> Review {
        let mut notes = self.notes.clone();
        notes.sort_by_key(|note| block_of(&note.target));
        Review {
            subject: self.subject.clone(),
            round: self.round,
            global: self.global.clone(),
            notes: Notes::Plan(notes),
        }
    }

    fn send(&mut self, asked: Verdict) {
        let review = self.review();
        if asked == Verdict::RequestChanges && !review.has_notes() {
            self.dialog = Dialog::None;
            self.toast = Some("add a note first, so the agent knows what to change".into());
            return;
        }
        let verdict = review.verdict(asked);
        self.outcome = Some(Outcome::Sent {
            verdict,
            text: review.render(asked),
        });
        (self.save)(None);
    }

    fn notes_key(&mut self, key: KeyEvent, selected: usize) {
        let listed = self.listed();
        let picked = listed.get(selected).copied();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('L') => self.dialog = Dialog::None,
            KeyCode::Char('j') | KeyCode::Down => {
                let selected = (selected + 1).min(listed.len().saturating_sub(1));
                self.dialog = Dialog::Notes { selected };
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.dialog = Dialog::Notes {
                    selected: selected.saturating_sub(1),
                };
            }
            KeyCode::Enter => {
                if let Some(at) = picked {
                    let block = block_of(&self.notes[at].target);
                    self.dialog = Dialog::None;
                    self.set_focus_index(block as isize);
                }
            }
            KeyCode::Char('e') => {
                if let Some(at) = picked {
                    self.edit_note(at);
                }
            }
            KeyCode::Char('x') | KeyCode::Char('d') => {
                if let Some(at) = picked {
                    self.delete_note(at);
                    let left = self.notes.len();
                    self.dialog = if left == 0 {
                        Dialog::None
                    } else {
                        Dialog::Notes {
                            selected: selected.min(left - 1),
                        }
                    };
                }
            }
            _ => {}
        }
    }

    fn key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.composer.is_some() {
            return self.composer_key(key);
        }
        match self.dialog {
            Dialog::Help => {
                self.dialog = Dialog::None;
                return;
            }
            Dialog::Verdict => {
                match key.code {
                    KeyCode::Char('a') => self.send(Verdict::Approve),
                    KeyCode::Char('n') => self.send(Verdict::ApproveWithNotes),
                    KeyCode::Char('c') | KeyCode::Char('r') => self.send(Verdict::RequestChanges),
                    KeyCode::Esc | KeyCode::Char('q') => self.dialog = Dialog::None,
                    _ => {}
                }
                return;
            }
            Dialog::Quit => {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('q') => self.outcome = Some(Outcome::Closed),
                    KeyCode::Char('S') => self.dialog = Dialog::Verdict,
                    _ => self.dialog = Dialog::None,
                }
                return;
            }
            Dialog::Notes { selected } => return self.notes_key(key, selected),
            Dialog::None => {}
        }
        self.toast = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let block = match self.focus {
            Focus::Block(block) => Some(block),
            Focus::Global => None,
        };
        // Moving the cursor lets go of a selection.
        if matches!(
            key.code,
            KeyCode::Char('j' | 'k' | 'J' | 'K' | 'g' | 'G') | KeyCode::Down | KeyCode::Up
        ) {
            self.selection = None;
        }
        match key.code {
            KeyCode::Esc if self.selection.is_some() => self.selection = None,
            KeyCode::Char('c') if ctrl => self.outcome = Some(Outcome::Closed),
            KeyCode::Char('j') | KeyCode::Down => self.step(1),
            KeyCode::Char('k') | KeyCode::Up => self.step(-1),
            KeyCode::Char('J') => self.step_noted(true),
            KeyCode::Char('K') => self.step_noted(false),
            KeyCode::Char('d') if ctrl => self.step(HALF_STEP as isize),
            KeyCode::Char('u') if ctrl => self.step(-(HALF_STEP as isize)),
            KeyCode::Char(' ') | KeyCode::PageDown => {
                self.scroll_by(self.view_height.saturating_sub(2) as isize)
            }
            KeyCode::Char('b') | KeyCode::PageUp => {
                self.scroll_by(-(self.view_height.saturating_sub(2) as isize))
            }
            KeyCode::Char('g') | KeyCode::Home => self.set_focus_index(-1),
            KeyCode::Char('G') | KeyCode::End => self.set_focus_index(self.blocks.len() as isize),
            KeyCode::Char('c') | KeyCode::Enter => self.compose_comment(),
            KeyCode::Char('C') => self.compose_global(),
            KeyCode::Char('x') => self.remove_here(),
            KeyCode::Char('r') => self.compose_replace(),
            KeyCode::Char('X') => match block {
                Some(block) => self.clear_block(block),
                None => {
                    self.global = None;
                    self.toast = Some("global note cleared".into());
                    self.persist();
                }
            },
            KeyCode::Char('L') => {
                if self.notes.is_empty() {
                    self.toast = Some("no notes yet".into());
                } else {
                    self.dialog = Dialog::Notes { selected: 0 };
                }
            }
            KeyCode::Char('S') => self.dialog = Dialog::Verdict,
            KeyCode::Char('?') => self.dialog = Dialog::Help,
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.note_count() > 0 {
                    self.dialog = Dialog::Quit;
                } else {
                    self.outcome = Some(Outcome::Closed);
                }
            }
            _ => {}
        }
    }

    fn composer_key(&mut self, key: KeyEvent) {
        let Some(composer) = self.composer.as_mut() else {
            return;
        };
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.composer = None;
                self.toast = Some("discarded".into());
            }
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                composer.text.push('\n')
            }
            KeyCode::Enter => self.save_composer(),
            KeyCode::Backspace if alt || ctrl => {
                let kept = composer.text.trim_end().len();
                let word = composer.text[..kept]
                    .rfind(char::is_whitespace)
                    .map_or(0, |at| at + 1);
                composer.text.truncate(word);
            }
            KeyCode::Backspace => {
                composer.text.pop();
            }
            KeyCode::Char('u') if ctrl => composer.text.clear(),
            KeyCode::Char(c) if !ctrl => composer.text.push(c),
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        if let Some(composer) = self.composer.as_mut() {
            composer.text.push_str(&text.replace('\r', ""));
        }
    }

    fn mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_STEP as isize),
            MouseEventKind::ScrollUp => self.scroll_by(-(WHEEL_STEP as isize)),
            _ if self.composer.is_some() || self.dialog != Dialog::None => {}
            MouseEventKind::Down(MouseButton::Left) => {
                self.selection = None;
                self.drag = None;
                let Some(row) = self.row_at(mouse.row) else {
                    return;
                };
                if let Some(focus) = row.focus() {
                    self.focus = focus;
                }
                if let Row::Text { block, row } = row {
                    self.drag = self
                        .offset_at(block, row, mouse.column)
                        .map(|offset| (block, offset));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((block, from)) = self.drag else {
                    return;
                };
                // A selection stays inside the block it started in.
                let to = match self.row_at(mouse.row) {
                    Some(Row::Text { block: b, row }) if b == block => {
                        self.offset_at(block, row, mouse.column)
                    }
                    _ => None,
                };
                if let Some(to) = to {
                    let (start, end) = if to >= from {
                        (from, to + 1)
                    } else {
                        (to, from + 1)
                    };
                    self.selection = Some(Selection { block, start, end });
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag = None;
                if let Some(selection) = self.selection
                    && self.selected_text(selection).trim().is_empty()
                {
                    self.selection = None;
                }
                if self.selection.is_some() {
                    self.toast = Some("selected · c comment · x remove · r replace".into());
                }
            }
            _ => {}
        }
    }

    fn row_at(&self, y: u16) -> Option<Row> {
        self.shown
            .iter()
            .find(|(at, _)| *at == y)
            .map(|(_, row)| *row)
    }

    /// The character of block `block`'s text drawn at screen column `x` on
    /// its row `row`: the nearest one when `x` is past either end.
    fn offset_at(&self, block: usize, row: usize, x: u16) -> Option<usize> {
        let line = self.laid.get(block)?.rows.get(row)?;
        let x = x.saturating_sub(self.text_x);
        let mut nearest = None;
        for seg in &line.segs {
            let Some(start) = seg.offset else { continue };
            let mut col = seg.x;
            for (n, c) in seg.text.chars().enumerate() {
                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
                if x < col + w.max(1) {
                    return Some(nearest.filter(|_| x < seg.x).unwrap_or(start + n));
                }
                col += w;
                nearest = Some(start + n);
            }
        }
        nearest
    }

    /// Ranges of block `block`'s text to mark: passages with notes, in their
    /// note's color.
    fn passage_marks(&self, block: usize) -> Vec<(usize, usize, Color)> {
        let Some(laid) = self.laid.get(block) else {
            return Vec::new();
        };
        let text: String = laid.text.iter().collect();
        self.notes_on(block)
            .filter_map(|(_, note)| match &note.target {
                PlanTarget::Passage { quote, .. } => {
                    let at = text.find(quote.as_str())?;
                    let start = text[..at].chars().count();
                    Some((start, start + quote.chars().count(), hue_of(&note.edit)))
                }
                PlanTarget::Block { .. } => None,
            })
            .collect()
    }

    // Drawing

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        if area.height < 4 || area.width < 20 {
            f.render_widget(Paragraph::new("leaf: window too small"), area);
            return;
        }
        let composer_height = self.composer.as_ref().map_or(0, |c| {
            let width = area.width.saturating_sub(4).max(1) as usize;
            (wrapped(&c.text, width).len() as u16 + 2).clamp(3, area.height / 2)
        });
        let header = Rect { height: 1, ..area };
        let footer = Rect {
            y: area.bottom() - 1,
            height: 1,
            ..area
        };
        let body = Rect {
            y: area.y + 2,
            height: area.height.saturating_sub(3 + composer_height),
            ..area
        };
        let column_width = area.width.saturating_sub(GUTTER + 2).min(MAX_TEXT_WIDTH);
        let left = area.x + area.width.saturating_sub(column_width + GUTTER) / 2;
        self.lay_out(column_width);
        self.view_height = body.height as usize;
        self.follow();
        self.draw_header(f, header);
        self.draw_body(f, body, left, column_width);
        if composer_height > 0 {
            let at = Rect {
                y: footer.y - composer_height,
                height: composer_height,
                ..area
            };
            self.draw_composer(f, at);
        }
        self.draw_footer(f, footer);
        match self.dialog {
            Dialog::None => {}
            Dialog::Help => draw_help(f, area),
            Dialog::Verdict => self.draw_verdict(f, area),
            Dialog::Quit => self.draw_quit(f, area),
            Dialog::Notes { selected } => self.draw_notes(f, area, selected),
        }
    }

    fn draw_notes(&self, f: &mut Frame, area: Rect, selected: usize) {
        let mut lines = vec![
            Line::from(Span::styled(
                format!("Notes ({})", self.notes.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        let width = area.width.saturating_sub(8).min(76) as usize;
        for (row, at) in self.listed().into_iter().enumerate() {
            let note = &self.notes[at];
            let text = format!(
                "{} {} · {}",
                if row == selected { "›" } else { " " },
                target_label(&note.target),
                note_summary(note)
            );
            let mut style = Style::default().fg(hue_of(&note.edit));
            if row == selected {
                style = style.add_modifier(Modifier::BOLD | Modifier::REVERSED);
            }
            lines.push(Line::from(Span::styled(clip(&text, width), style)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "enter go to · e edit · x delete · esc close",
            Style::default().fg(DIM),
        )));
        popup(f, area, lines, width as u16 + 4);
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let round = self
            .round
            .filter(|round| *round > 1)
            .map(|round| format!(" · round {round}"))
            .unwrap_or_default();
        let left = format!(" Review plan · {}{round}", self.subject);
        let right = format!("{} blocks ", self.blocks.len());
        let pad = (area.width as usize).saturating_sub(left.width() + right.width());
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" ".repeat(pad)),
                Span::styled(right, Style::default().fg(DIM)),
            ])),
            area,
        );
    }

    fn draw_body(&mut self, f: &mut Frame, area: Rect, left: u16, width: u16) {
        let rows = self.rows();
        self.shown.clear();
        let text_x = left + GUTTER;
        self.text_x = text_x;
        for (y, row) in (area.y..area.bottom()).zip(rows.iter().skip(self.scroll)) {
            self.shown.push((y, *row));
            let focused = row.focus() == Some(self.focus);
            let buf = f.buffer_mut();
            if focused {
                buf.set_string(left + GUTTER - 2, y, "▌", Style::default().fg(ACCENT));
            }
            match *row {
                Row::GlobalTitle => {
                    let style = if focused {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(DIM)
                    };
                    let hint = if self.global.is_some() {
                        "Global note"
                    } else {
                        "Global note · C to write one"
                    };
                    buf.set_string(text_x, y, hint, style);
                }
                Row::GlobalNote => {
                    let text = self.global.as_deref().unwrap_or("");
                    let first = text.lines().next().unwrap_or("");
                    buf.set_string(
                        text_x,
                        y,
                        clip(&format!("● {first}"), width as usize),
                        Style::default().fg(Hue::Purple.marker()),
                    );
                }
                Row::Text { block, row } => {
                    if row == 0 {
                        let number = format!("{:>3}", block + 1);
                        let style = if focused {
                            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(DIM)
                        };
                        buf.set_string(left, y, number, style);
                    }
                    let crossed = self.removed(block);
                    let marks = self.passage_marks(block);
                    let selected = self
                        .selection
                        .filter(|s| s.block == block)
                        .map(|s| (s.start, s.end));
                    let buf = f.buffer_mut();
                    if let Some(line) = self.laid[block].rows.get(row) {
                        for seg in &line.segs {
                            let mut x = text_x + seg.x;
                            for (n, c) in seg.text.chars().enumerate() {
                                let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                                if x + w as u16 > text_x + width {
                                    break;
                                }
                                let mut style = seg.style;
                                if crossed {
                                    style = style.add_modifier(Modifier::CROSSED_OUT).fg(DIM);
                                }
                                if let Some(offset) = seg.offset.map(|o| o + n) {
                                    if let Some((_, _, hue)) =
                                        marks.iter().find(|(s, e, _)| *s <= offset && offset < *e)
                                    {
                                        style = style.fg(*hue).add_modifier(Modifier::UNDERLINED);
                                    }
                                    if selected.is_some_and(|(s, e)| s <= offset && offset < e) {
                                        style = style.bg(SELECTION);
                                    }
                                }
                                buf.set_string(x, y, c.to_string(), style);
                                x += w as u16;
                            }
                        }
                    }
                }
                Row::Note { note, .. } => {
                    let note = &self.notes[note];
                    let text = format!("● {}", note_summary(note));
                    buf.set_string(
                        text_x,
                        y,
                        clip(&text, width as usize),
                        Style::default().fg(hue_of(&note.edit)),
                    );
                }
                Row::Gap => {}
            }
        }
    }

    fn draw_composer(&self, f: &mut Frame, area: Rect) {
        let Some(composer) = &self.composer else {
            return;
        };
        let title = match &composer.writing {
            Writing::Comment { target, .. } => format!(" Comment on {} ", target_label(target)),
            Writing::Replace { target, .. } => format!(" Replace {} ", target_label(target)),
            Writing::Global => " Global note ".to_owned(),
        };
        let frame = Frame_::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(ACCENT))
            .title(title);
        let inner = frame.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(frame, area);
        let lines = wrapped(&format!("{}▏", composer.text), inner.width.max(1) as usize);
        let skip = lines.len().saturating_sub(inner.height as usize);
        let shown: Vec<Line> = lines.into_iter().skip(skip).map(Line::from).collect();
        f.render_widget(Paragraph::new(shown), inner);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let left = if let Some(toast) = &self.toast {
            format!(" {toast}")
        } else if self.composer.is_some() {
            " enter save · alt+enter new line · esc discard".to_owned()
        } else {
            " j/k blocks · c comment · x remove · r replace · C global · L notes · S send · ? keys"
                .to_owned()
        };
        let count = self.note_count();
        let right = match count {
            0 => "no notes ".to_owned(),
            1 => "1 note ".to_owned(),
            n => format!("{n} notes "),
        };
        let room = (area.width as usize).saturating_sub(right.width() + 1);
        let left = clip(&left, room);
        let pad = (area.width as usize).saturating_sub(left.width() + right.width());
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(left, Style::default().fg(DIM)),
                Span::raw(" ".repeat(pad)),
                Span::styled(right, Style::default().fg(DIM)),
            ])),
            area,
        );
    }

    fn draw_verdict(&self, f: &mut Frame, area: Rect) {
        let review = self.review();
        let count = self.note_count();
        let notes = match count {
            0 => "no notes".to_owned(),
            1 => "1 note".to_owned(),
            n => format!("{n} notes"),
        };
        let approve = if review.has_notes() {
            "  a  approve, with these notes"
        } else {
            "  a  approve"
        };
        let lines = vec![
            Line::from(Span::styled(
                format!("Send the review ({notes})"),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(approve),
            Line::from("  n  approve with notes"),
            Line::from("  c  request changes"),
            Line::from(""),
            Line::from(Span::styled(
                "  esc  keep reviewing",
                Style::default().fg(DIM),
            )),
        ];
        popup(f, area, lines, 40);
    }

    fn draw_quit(&self, f: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from(Span::styled(
                format!("{} notes aren't sent yet", self.note_count()),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  S    send them"),
            Line::from("  y    quit; they stay for next time"),
            Line::from(Span::styled(
                "  esc  keep reviewing",
                Style::default().fg(DIM),
            )),
        ];
        popup(f, area, lines, 40);
    }
}

/// The text a note's target covers: the whole block, or the passage.
fn target_text(target: &PlanTarget) -> &str {
    match target {
        PlanTarget::Block { text, .. } => text,
        PlanTarget::Passage { quote, .. } => quote,
    }
}

fn target_label(target: &PlanTarget) -> String {
    match target {
        PlanTarget::Block { index, .. } => format!("block {index}"),
        PlanTarget::Passage { block, .. } => format!("a passage in block {block}"),
    }
}

/// One line about a note: what it asks, and its text's first line.
fn note_summary(note: &PlanNote) -> String {
    let (label, body) = match &note.edit {
        PlanEdit::Comment(body) => ("comment", body.as_str()),
        PlanEdit::Remove => match &note.target {
            PlanTarget::Block { .. } => ("remove", "this block"),
            PlanTarget::Passage { quote, .. } => ("remove", quote.as_str()),
        },
        PlanEdit::Replace(with) => ("replace", with.as_str()),
    };
    let first = body.lines().next().unwrap_or("");
    let more = if body.lines().nth(1).is_some() {
        " …"
    } else {
        ""
    };
    format!("{label}  {first}{more}")
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("1 {word}")
    } else {
        format!("{count} {word}s")
    }
}

fn block_of(target: &PlanTarget) -> usize {
    match target {
        PlanTarget::Block { index, .. } => index.saturating_sub(1),
        PlanTarget::Passage { block, .. } => block.saturating_sub(1),
    }
}

fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out
}

/// `text` broken into lines no wider than `width`, keeping its own line breaks.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        let mut used = 0;
        for c in line.chars() {
            let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if used + w > width {
                out.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(c);
            used += w;
        }
        out.push(current);
    }
    out
}

fn popup(f: &mut Frame, area: Rect, lines: Vec<Line>, width: u16) {
    let width = width.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(area.height);
    let at = Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    f.render_widget(Clear, at);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Frame_::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(ACCENT)),
        ),
        at,
    );
}

const KEYS: &[(&str, &str)] = &[
    ("j/k ↓/↑", "next or previous block"),
    ("J/K", "next or previous block with notes"),
    ("g/G", "global note, last block"),
    ("ctrl+d/u", "five blocks down or up"),
    ("space/b", "a screen down or up"),
    ("c, enter", "comment on the block"),
    ("x", "mark the block for removal (again: keep it)"),
    ("r", "replace the block with your text"),
    ("C", "global note on the whole plan"),
    ("X", "clear the block's notes"),
    ("L", "every note: go to, edit or delete one"),
    ("drag", "select a passage; c, x and r then act on it"),
    ("S", "send the review with a verdict"),
    ("q", "quit"),
];

fn draw_help(f: &mut Frame, area: Rect) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!("{keys:>10}  "), Style::default().fg(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    popup(f, area, lines, 60);
}

/// Opens the plan for review and says how it was left.
pub fn run(plan: Plan) -> io::Result<Outcome> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    let mut terminal = ratatui::init();
    let title = format!("Review {}", plan.subject);
    let setup = execute!(
        io::stdout(),
        EnableMouseCapture,
        crossterm::event::EnableBracketedPaste,
        SetTitle(&title)
    );
    let mut app = App::new(plan);
    let result = setup.and_then(|_| event_loop(&mut app, &mut terminal));
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result.map(|()| app.outcome.unwrap_or(Outcome::Closed))
}

fn event_loop(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> io::Result<()> {
    loop {
        terminal.draw(|f| app.draw(f))?;
        if app.outcome.is_some() {
            return Ok(());
        }
        if !event::poll(Duration::from_millis(500))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) => app.key(key),
            Event::Mouse(mouse) => app.mouse(mouse),
            Event::Paste(text) => app.paste(&text),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;
    use std::rc::Rc;

    const PLAN: &str = "# Auth

Move sessions to tokens.

- Add the tokens table
- Backfill it

```sql
create table tokens (id int);
```
";

    fn app() -> App {
        App::new(Plan {
            subject: "plan.md".into(),
            source: PLAN.into(),
            round: None,
            draft: None,
            save: Box::new(|_| {}),
        })
    }

    /// An app whose saved drafts land in the returned cell.
    fn saving(source: &str, draft: Option<String>) -> (App, Rc<RefCell<Option<String>>>) {
        let saved = Rc::new(RefCell::new(None));
        let sink = saved.clone();
        let app = App::new(Plan {
            subject: "plan.md".into(),
            source: source.into(),
            round: None,
            draft,
            save: Box::new(move |json| *sink.borrow_mut() = json.map(str::to_owned)),
        });
        (app, saved)
    }

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }

    /// The screen position of the first character of `needle`.
    fn find(screen: &str, needle: &str) -> (u16, u16) {
        for (y, line) in screen.lines().enumerate() {
            if let Some(at) = line.find(needle) {
                return (line[..at].chars().count() as u16, y as u16);
            }
        }
        panic!("{needle:?} isn't on screen:\n{screen}");
    }

    #[test]
    fn a_dragged_passage_gets_its_own_note_with_its_source_line() {
        let mut app = app();
        let screen = draw(&mut app, 90, 30);
        let (x, y) = find(&screen, "sessions");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x, y);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), x + 7, y);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), x + 7, y);
        assert_eq!(
            app.selection.map(|s| app.selected_text(s)),
            Some("sessions".to_owned())
        );
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "Which sessions?");
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.selection, None);
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        assert!(
            sent(&app).contains("## 1. Block 1, line 3: comment\n> sessions\n\nWhich sessions?\n"),
            "{}",
            sent(&app)
        );
    }

    #[test]
    fn x_on_a_selection_removes_only_that_passage() {
        let mut app = app();
        let screen = draw(&mut app, 90, 30);
        let (x, y) = find(&screen, "Backfill");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), x, y);
        mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), x + 7, y);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), x + 7, y);
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.removed(2));
        assert_eq!(app.notes.len(), 1);
        assert!(matches!(
            &app.notes[0].target,
            PlanTarget::Passage { quote, block: 3, lines: Some((6, 6)) } if quote == "Backfill"
        ));
    }

    #[test]
    fn the_notes_list_edits_and_deletes_notes() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('L'));
        assert_eq!(app.dialog, Dialog::Notes { selected: 0 });
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(
            app.composer.as_ref().map(|c| c.text.as_str()),
            Some("first")
        );
        type_text(&mut app, " and better");
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            app.notes[0].edit,
            PlanEdit::Comment("first and better".into())
        );
        assert_eq!(app.notes.len(), 2);
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.notes.len(), 1);
        assert_eq!(app.dialog, Dialog::Notes { selected: 0 });
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.dialog, Dialog::None);
    }

    #[test]
    fn unsent_notes_are_saved_and_come_back() {
        let (mut first, saved) = saving(PLAN, None);
        press(&mut first, KeyCode::Char('C'));
        type_text(&mut first, "whole plan");
        press(&mut first, KeyCode::Enter);
        press(&mut first, KeyCode::Char('j'));
        press(&mut first, KeyCode::Char('c'));
        type_text(&mut first, "on block 2");
        press(&mut first, KeyCode::Enter);
        let draft = saved.borrow().clone();
        assert!(draft.is_some());
        let (second, _) = saving(PLAN, draft);
        assert_eq!(second.global.as_deref(), Some("whole plan"));
        assert_eq!(second.notes, first.notes);
        assert_eq!(second.toast.as_deref(), Some("2 notes from last time"));
    }

    #[test]
    fn a_note_whose_block_changed_is_dropped_and_sending_clears_the_draft() {
        let (mut first, saved) = saving(PLAN, None);
        press(&mut first, KeyCode::Char('j'));
        press(&mut first, KeyCode::Char('c'));
        type_text(&mut first, "keep");
        press(&mut first, KeyCode::Enter);
        press(&mut first, KeyCode::Char('j'));
        press(&mut first, KeyCode::Char('x'));
        let draft = saved.borrow().clone();
        let changed = PLAN.replace("- Backfill it", "- Backfill it in batches");
        let (mut second, saved) = saving(&changed, draft);
        assert_eq!(second.notes.len(), 1);
        assert!(
            second
                .toast
                .as_deref()
                .is_some_and(|t| t.contains("1 no longer match")),
            "{:?}",
            second.toast
        );
        press(&mut second, KeyCode::Char('S'));
        press(&mut second, KeyCode::Char('c'));
        assert_eq!(*saved.borrow(), None);
    }

    fn press(app: &mut App, code: KeyCode) {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|f| app.draw(f)).expect("draws");
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sent(app: &App) -> &str {
        match &app.outcome {
            Some(Outcome::Sent { text, .. }) => text,
            other => panic!("expected a sent review, got {other:?}"),
        }
    }

    #[test]
    fn the_plan_shows_numbered_blocks_and_a_global_row() {
        let mut app = app();
        let screen = draw(&mut app, 90, 30);
        assert!(screen.contains("Review plan · plan.md"), "{screen}");
        assert!(screen.contains("Global note · C to write one"), "{screen}");
        for number in ["  1", "  2", "  3", "  4"] {
            assert!(
                screen
                    .lines()
                    .any(|l| l.trim_start().starts_with(number.trim()))
            );
        }
        assert!(screen.contains("Move sessions to tokens."));
        assert!(screen.contains("4 blocks"));
    }

    #[test]
    fn a_comment_on_a_block_is_sent_with_the_verdict() {
        let mut app = app();
        draw(&mut app, 90, 30);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.focus, Focus::Block(1));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "Use a migration");
        press(&mut app, KeyCode::Enter);
        let screen = draw(&mut app, 90, 30);
        assert!(screen.contains("● comment  Use a migration"), "{screen}");
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        let text = sent(&app);
        assert!(
            text.starts_with("# Review of plan.md: changes requested"),
            "{text}"
        );
        assert!(text.contains("## 1. Block 2, \"Auth\" (line 5): comment\n> - Add the tokens table\n\nUse a migration\n"), "{text}");
    }

    #[test]
    fn approving_with_notes_and_without() {
        let mut app = app();
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.outcome,
            Some(Outcome::Sent {
                verdict: Verdict::Approve,
                text: "# Review of plan.md: approved\n\nApproved. Go ahead.\n".into()
            })
        );
        let mut app = super::tests::app();
        press(&mut app, KeyCode::Char('C'));
        type_text(&mut app, "Log it");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('a'));
        assert!(matches!(
            app.outcome,
            Some(Outcome::Sent {
                verdict: Verdict::ApproveWithNotes,
                ..
            })
        ));
        assert!(sent(&app).contains("Global: Log it"));
    }

    #[test]
    fn requesting_changes_needs_a_note() {
        let mut app = app();
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        assert_eq!(app.outcome, None);
        assert_eq!(app.dialog, Dialog::None);
        assert!(
            app.toast
                .as_deref()
                .is_some_and(|t| t.contains("add a note"))
        );
    }

    #[test]
    fn x_marks_a_block_for_removal_and_again_keeps_it() {
        let mut app = app();
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.focus, Focus::Block(3));
        press(&mut app, KeyCode::Char('x'));
        assert!(app.removed(3));
        press(&mut app, KeyCode::Char('x'));
        assert!(!app.removed(3));
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        assert!(
            sent(&app).contains("(lines 8-10): remove"),
            "{}",
            sent(&app)
        );
    }

    #[test]
    fn replace_starts_from_the_block_and_unchanged_text_saves_nothing() {
        let mut app = app();
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(
            app.composer.as_ref().map(|c| c.text.as_str()),
            Some("# Auth\n\nMove sessions to tokens.")
        );
        press(&mut app, KeyCode::Enter);
        assert!(app.notes.is_empty());
        press(&mut app, KeyCode::Char('r'));
        if let Some(composer) = app.composer.as_mut() {
            composer.text = "# Auth\n\nMove sessions to signed tokens.".into();
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.notes.len(), 1);
        assert!(matches!(app.notes[0].edit, PlanEdit::Replace(_)));
    }

    #[test]
    fn quitting_with_unsent_notes_asks_first() {
        let mut app = app();
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "hm");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.dialog, Dialog::Quit);
        assert_eq!(app.outcome, None);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.dialog, Dialog::None);
        press(&mut app, KeyCode::Char('q'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.outcome, Some(Outcome::Closed));
        let mut empty = super::tests::app();
        press(&mut empty, KeyCode::Char('q'));
        assert_eq!(empty.outcome, Some(Outcome::Closed));
    }

    #[test]
    fn notes_are_sent_in_block_order_whatever_order_they_were_written() {
        let mut app = app();
        press(&mut app, KeyCode::Char('G'));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "last");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Char('c'));
        type_text(&mut app, "first");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('S'));
        press(&mut app, KeyCode::Char('c'));
        let text = sent(&app);
        let first = text.find("first").unwrap_or(usize::MAX);
        let last = text.find("last").unwrap_or(0);
        assert!(first < last, "{text}");
    }

    #[test]
    fn j_and_k_stop_at_the_ends() {
        let mut app = app();
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.focus, Focus::Global);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.focus, Focus::Global);
        for _ in 0..10 {
            press(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.focus, Focus::Block(3));
    }
}
