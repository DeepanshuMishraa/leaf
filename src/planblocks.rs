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

/// The plan's blocks in order: each heading with everything under it up to
/// the next heading or rule, and the content before the first heading (or
/// between a rule and a heading) as a block of its own. Every line of content
/// is in exactly one block; front matter, rules and blank lines are in none.
pub fn blocks(source: &str) -> Vec<Block> {
    let lines = Lines::new(source);
    let mut out: Vec<Block> = Vec::new();
    let mut path: Vec<(HeadingLevel, String)> = Vec::new();
    // Whether the last block takes what comes next.
    let mut open = false;
    for (piece, range) in pieces(source) {
        let span = lines.span(source, &range);
        let kind = match piece {
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
                open = true;
                continue;
            }
            Piece::Rule => {
                open = false;
                continue;
            }
            Piece::Paragraph => BlockKind::Paragraph,
            Piece::Other(kind) => kind,
        };
        match out.last_mut() {
            Some(last) if open => last.lines.1 = span.1,
            _ => {
                open = true;
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
        for marker in [
            "- [ ] ", "- [x] ", "- [X] ", "[ ] ", "[x] ", "[X] ", "* ", "- ", "+ ",
        ] {
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
    let unmarked: String = links(text)
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '`' | '~'))
        .collect();
    unmarked.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `[label](target)` becomes `label`, so a quote copied from the rendered
/// plan matches the link syntax in its source. Inline code renders as
/// written, so link syntax inside it stays; and a `[` opens a link only when
/// its `](` comes before any other `[`.
fn links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(['[', '`']) {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        if tail.starts_with('`') {
            let ticks = tail.chars().take_while(|&c| c == '`').count();
            let fence = &tail[..ticks];
            match tail[ticks..].find(fence) {
                Some(end) => {
                    let span = ticks + end + ticks;
                    out.push_str(&tail[..span]);
                    rest = &tail[span..];
                }
                None => {
                    out.push_str(fence);
                    rest = &tail[ticks..];
                }
            }
            continue;
        }
        let after = &tail[1..];
        let link = label_end(after).and_then(|close| {
            let end = after[close + 2..].find(')')?;
            Some((close, close + 2 + end + 1))
        });
        match link {
            Some((close, next)) => {
                out.push_str(&after[..close]);
                rest = &after[next..];
            }
            None => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Where a link label that starts `text` ends: the `](` after it, looked
/// for outside inline code, so a label may hold code. `None` when another
/// `[` comes first or there's no `](`.
fn label_end(text: &str) -> Option<usize> {
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with("](") {
            return Some(at);
        }
        if rest.starts_with('[') {
            return None;
        }
        if rest.starts_with('`') {
            let ticks = rest.chars().take_while(|&c| c == '`').count();
            at += match rest[ticks..].find(&rest[..ticks]) {
                Some(end) => ticks + end + ticks,
                None => ticks,
            };
            continue;
        }
        at += rest.chars().next().map_or(1, char::len_utf8);
    }
    None
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
    fn a_plan_splits_into_a_block_per_heading_and_one_after_a_rule() {
        let blocks = blocks(PLAN);
        assert_eq!(
            kinds(&blocks),
            vec![
                (BlockKind::Section, (5, 8)),
                (BlockKind::Section, (10, 24)),
                (BlockKind::Paragraph, (28, 28)),
            ]
        );
        assert_eq!(
            blocks.iter().map(|b| b.index).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            blocks[0].text,
            "# Auth rework\n\nWe move sessions to tokens.\nTwo steps."
        );
        assert!(
            blocks[1]
                .text
                .starts_with("## Phase 1\n\n- Add the `tokens` table\n")
        );
        assert!(blocks[1].text.ends_with("> Ask the security team first."));
    }

    #[test]
    fn content_before_the_first_heading_is_one_block() {
        let blocks = blocks("Intro line.\n\n- one\n- two\n\n# Then\n\nmore\n");
        assert_eq!(
            kinds(&blocks),
            vec![(BlockKind::Paragraph, (1, 4)), (BlockKind::Section, (6, 8))]
        );
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
    fn link_syntax_is_unwrapped_only_where_it_renders_as_a_link() {
        assert_eq!(
            links("Use [1] and see [docs](https://x)"),
            "Use [1] and see docs"
        );
        assert_eq!(
            links("Write `[a](b)` for [a link](c)"),
            "Write `[a](b)` for a link"
        );
        assert_eq!(
            links("Run [the `leaf` command](https://x) now"),
            "Run the `leaf` command now"
        );
        // Quoting the second line's link text must not land on the code
        // example above it.
        let source = "Write `[a](b)` here\nWrite a here";
        assert_eq!(locate(source, "Write a here"), Some((2, 2)));
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
