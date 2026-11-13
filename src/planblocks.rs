//! Splits a plan's Markdown into the blocks Margin reviews one at a time, and
//! finds where a quoted passage sits in the source.

use std::ops::Range;

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::render::MARKDOWN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// A heading and the paragraphs under it.
    Section,
    /// A top-level list item, with anything nested in it.
    Item,
    Code,
    Table,
    Quote,
    Paragraph,
    /// Raw HTML, math, a definition list or anything else on its own.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// 1-based, as the gutter shows it.
    pub index: usize,
    /// Source lines, 1-based and inclusive.
    pub lines: (usize, usize),
    /// The headings this block sits under, outermost first; a section's own
    /// heading is the last.
    pub heading: Vec<String>,
    pub kind: BlockKind,
    /// The block's source lines.
    pub text: String,
}

/// A top-level piece of the document, before paragraphs join their heading.
enum Piece {
    Heading(HeadingLevel, String),
    Paragraph,
    Rule,
    Other(BlockKind),
}

fn kind_of(tag: &Tag) -> BlockKind {
    match tag {
        Tag::CodeBlock(_) => BlockKind::Code,
        Tag::Table(_) => BlockKind::Table,
        Tag::BlockQuote(_) => BlockKind::Quote,
        Tag::Paragraph => BlockKind::Paragraph,
        Tag::Item => BlockKind::Item,
        _ => BlockKind::Other,
    }
}

/// The top-level pieces and their byte ranges. A list gives one piece per item.
fn pieces(source: &str) -> Vec<(Piece, Range<usize>)> {
    let options = MARKDOWN | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
    let mut out = Vec::new();
    let mut depth = 0usize;
    // Set while inside a top-level list, so its items are taken one by one.
    let mut in_list = false;
    let mut heading: Option<(HeadingLevel, String, Range<usize>)> = None;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if depth == 0 {
                    match &tag {
                        Tag::Heading { level, .. } => {
                            heading = Some((*level, String::new(), range.clone()))
                        }
                        Tag::List(_) => in_list = true,
                        Tag::MetadataBlock(_) => {}
                        Tag::Paragraph => out.push((Piece::Paragraph, range.clone())),
                        other => out.push((Piece::Other(kind_of(other)), range.clone())),
                    }
                } else if depth == 1 && in_list && matches!(tag, Tag::Item) {
                    out.push((Piece::Other(BlockKind::Item), range.clone()));
                }
                depth += 1;
            }
            Event::End(end) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    match end {
                        TagEnd::Heading(_) => {
                            if let Some((level, text, range)) = heading.take() {
                                out.push((Piece::Heading(level, text.trim().to_owned()), range));
                            }
                        }
                        TagEnd::List(_) => in_list = false,
                        _ => {}
                    }
                }
            }
            Event::Text(text) | Event::Code(text) | Event::InlineMath(text) => {
                if let Some((_, heading_text, _)) = heading.as_mut() {
                    heading_text.push_str(&text);
                }
            }
            Event::Rule if depth == 0 => out.push((Piece::Rule, range)),
            _ => {}
        }
    }
    out
}

/// 1-based line of each byte offset.
struct Lines {
    starts: Vec<usize>,
}

impl Lines {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(at, _)| at + 1));
        Lines { starts }
    }

    fn line(&self, byte: usize) -> usize {
        self.starts.partition_point(|start| *start <= byte)
    }

    /// The first and last line a byte range covers, ignoring a trailing newline.
    fn span(&self, source: &str, range: &Range<usize>) -> (usize, usize) {
        let text = &source[range.clone()];
        let end = range.start + text.trim_end().len();
        let first = self.line(range.start);
        (
            first,
            self.line(end.saturating_sub(1).max(range.start)).max(first),
        )
    }
}

fn heading_of(path: &[(HeadingLevel, String)]) -> Vec<String> {
    path.iter().map(|(_, text)| text.clone()).collect()
}

fn source_lines(source: &str, lines: (usize, usize)) -> String {
    source
        .lines()
        .skip(lines.0 - 1)
        .take(lines.1 + 1 - lines.0)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan's blocks in order. Every line of content is in exactly one block;
/// front matter, rules and blank lines are in none.
pub fn blocks(source: &str) -> Vec<Block> {
    let lines = Lines::new(source);
    let mut out: Vec<Block> = Vec::new();
    let mut path: Vec<(HeadingLevel, String)> = Vec::new();
    // A section takes the paragraphs straight after its heading.
    let mut open_section = false;
    for (piece, range) in pieces(source) {
        let span = lines.span(source, &range);
        match piece {
            Piece::Heading(level, text) => {
                while path.last().is_some_and(|(last, _)| *last >= level) {
                    path.pop();
                }
                path.push((level, text));
                out.push(Block {
                    index: 0,
                    lines: span,
                    heading: heading_of(&path),
                    kind: BlockKind::Section,
                    text: String::new(),
                });
                open_section = true;
            }
            Piece::Paragraph if open_section => {
                if let Some(section) = out.last_mut() {
                    section.lines.1 = span.1;
                }
            }
            Piece::Paragraph => out.push(Block {
                index: 0,
                lines: span,
                heading: heading_of(&path),
                kind: BlockKind::Paragraph,
                text: String::new(),
            }),
            Piece::Rule => open_section = false,
            Piece::Other(kind) => {
                open_section = false;
                out.push(Block {
                    index: 0,
                    lines: span,
                    heading: heading_of(&path),
                    kind,
                    text: String::new(),
                });
            }
        }
    }
    for (index, block) in out.iter_mut().enumerate() {
        block.index = index + 1;
        block.text = source_lines(source, block.lines);
    }
    out
}

/// A line with Markdown's marks taken off and its spaces squeezed, so a
/// passage copied from the rendered plan matches its source.
fn plain(line: &str) -> String {
    let mut text = line.trim_start();
    loop {
        let before = text;
        text = text
            .trim_start_matches('#')
            .trim_start_matches('>')
            .trim_start();
        for marker in ["- [ ] ", "- [x] ", "- [X] ", "* ", "- ", "+ "] {
            if let Some(rest) = text.strip_prefix(marker) {
                text = rest;
            }
        }
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && matches!(text[digits..].chars().next(), Some('.' | ')')) {
            text = text[digits + 1..].trim_start();
        }
        if text == before {
            break;
        }
    }
    let unmarked: String = text
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect();
    unmarked.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The source lines a quoted passage spans, when it can be found: its first
/// line's text on some line, and its last line's text on that line or after.
pub fn locate(source: &str, quote: &str) -> Option<(usize, usize)> {
    let wanted: Vec<String> = quote
        .lines()
        .map(plain)
        .filter(|line| !line.is_empty())
        .collect();
    let (first, last) = (wanted.first()?, wanted.last()?);
    let plains: Vec<String> = source.lines().map(plain).collect();
    let start = plains
        .iter()
        .position(|line| line.contains(first.as_str()))?;
    let end = plains[start..]
        .iter()
        .position(|line| line.contains(last.as_str()))?;
    Some((start + 1, start + end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "---
title: Auth
---

# Auth rework

We move sessions to tokens.
Two steps.

## Phase 1

- Add the `tokens` table
- Backfill it
  - in batches of **500**

```sql
create table tokens (id int);
```

| step | owner |
|---|---|
| 1 | me |

> Ask the security team first.

---

Loose closing paragraph.
";

    fn kinds(blocks: &[Block]) -> Vec<(BlockKind, (usize, usize))> {
        blocks.iter().map(|b| (b.kind, b.lines)).collect()
    }

    #[test]
    fn a_plan_splits_into_sections_items_and_standalone_blocks() {
        let blocks = blocks(PLAN);
        assert_eq!(
            kinds(&blocks),
            vec![
                (BlockKind::Section, (5, 8)),
                (BlockKind::Section, (10, 10)),
                (BlockKind::Item, (12, 12)),
                (BlockKind::Item, (13, 14)),
                (BlockKind::Code, (16, 18)),
                (BlockKind::Table, (20, 22)),
                (BlockKind::Quote, (24, 24)),
                (BlockKind::Paragraph, (28, 28)),
            ]
        );
        assert_eq!(
            blocks.iter().map(|b| b.index).collect::<Vec<_>>(),
            (1..=8).collect::<Vec<_>>()
        );
        assert_eq!(
            blocks[0].text,
            "# Auth rework\n\nWe move sessions to tokens.\nTwo steps."
        );
        assert_eq!(blocks[3].text, "- Backfill it\n  - in batches of **500**");
    }

    #[test]
    fn blocks_carry_the_headings_they_sit_under() {
        let blocks = blocks(PLAN);
        assert_eq!(blocks[0].heading, vec!["Auth rework"]);
        assert_eq!(blocks[1].heading, vec!["Auth rework", "Phase 1"]);
        assert_eq!(blocks[2].heading, vec!["Auth rework", "Phase 1"]);
        let sibling = super::blocks("## A\n\n### B\n\n## C\n");
        assert_eq!(sibling[2].heading, vec!["C"]);
    }

    #[test]
    fn every_content_line_is_in_exactly_one_block() {
        let blocks = blocks(PLAN);
        let skipped = [1, 2, 3, 26];
        for (number, line) in PLAN.lines().enumerate() {
            let number = number + 1;
            let holding = blocks
                .iter()
                .filter(|b| b.lines.0 <= number && number <= b.lines.1)
                .count();
            if line.trim().is_empty() || skipped.contains(&number) {
                continue;
            }
            assert_eq!(holding, 1, "line {number} {line:?}");
        }
    }

    #[test]
    fn an_empty_plan_has_no_blocks() {
        assert!(blocks("").is_empty());
        assert!(blocks("\n\n").is_empty());
    }

    #[test]
    fn a_passage_copied_from_the_rendered_plan_is_found_in_the_source() {
        assert_eq!(locate(PLAN, "in batches of 500"), Some((14, 14)));
        assert_eq!(
            locate(PLAN, "Add the tokens table\nBackfill it"),
            Some((12, 13))
        );
        assert_eq!(locate(PLAN, "move sessions"), Some((7, 7)));
        assert_eq!(locate(PLAN, "nowhere in the plan"), None);
        assert_eq!(locate(PLAN, "  \n "), None);
    }

    #[test]
    fn every_source_line_is_found_where_it_is() {
        for (number, line) in PLAN.lines().enumerate() {
            let text = plain(line);
            if text.is_empty() {
                continue;
            }
            let found = locate(PLAN, &text).map(|(first, _)| first);
            // An earlier line holding the same words is a fair answer too.
            let earliest = PLAN
                .lines()
                .map(plain)
                .position(|other| other.contains(&text))
                .map(|at| at + 1);
            assert_eq!(found, earliest, "line {} {line:?}", number + 1);
        }
    }
}
