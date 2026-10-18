//! PDF pages as markdown: text blocks become paragraphs, headings, lists,
//! tables and code; images and vector drawings become figures.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::LazyLock;

use image::RgbaImage;
use mupdf::{
    ColorParams, Colorspace, Device, Document, IRect, Matrix, MetadataName, Page, Pixmap, TextPageFlags,
    text_page::TextBlockType,
};
use regex::Regex;

use crate::doc::{Doc, Source};

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PdfOpenFailed {
    pub message: String,
}

/// One line of text. `top` is its top edge on the page, in PDF points; `left`
/// and `right` are its edges as shares of the page width; `mono` marks code.
/// Text on one row in separate runs (table cells, a list number and its item)
/// comes as separate lines with the same `top`.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub text: String,
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub top: f32,
    pub left: f32,
    pub right: f32,
    pub mono: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text(Vec<Line>),
    Image(String),
    /// A message from the reader itself, e.g. a page that failed to parse.
    Note(String),
}

/// A block once the page is laid out: tables found among the text.
#[derive(Debug, Clone)]
enum Laid {
    Text(Vec<Line>),
    Image(String),
    Note(String),
    Table(Vec<Vec<String>>),
}

/// Document-wide typography, learned from a sample of pages.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Font size of body text.
    pub body: f32,
    /// Sizes larger than body, biggest first; index 0 is `#`.
    pub heading_sizes: Vec<f32>,
    /// Usual top-to-top distance between lines inside a paragraph.
    pub pitch: f32,
    /// Normalized text of running headers and footers to drop.
    pub running: HashSet<String>,
    /// Usual width of a full line of body text, as a share of the page width.
    pub measure: f32,
}

/// Images smaller than this (in PDF points) are icons, bullets or rules, not content.
const MIN_IMAGE_SIZE: f32 = 40.0;
/// Embedded images are scaled down to this width; terminals never show more.
const MAX_IMAGE_PIXELS_WIDE: u32 = 800;
/// Widest an image is drawn, in pixels: a full text column on a large high-density screen.
const MAX_RENDER_PIXELS_WIDE: u32 = 3000;
/// Pages read to learn the layout, spread evenly through the document.
const SAMPLE_PAGES: usize = 24;
/// Shapes in a vector drawing; thinner paths are rules, underlines and borders.
const MIN_SHAPE_SIZE: f32 = 4.0;
/// A cluster of at least this many shapes is a drawing (one box is a shaded panel).
const MIN_FIGURE_SHAPES: usize = 3;
/// Shapes this close (in points) belong to the same drawing.
const FIGURE_GAP: f32 = 8.0;
/// Diagram labels are short; a line this long touching shapes means a boxed panel of prose.
const MAX_LABEL_LENGTH: usize = 60;
/// Labels this close (in points) to a drawing's shapes are part of it; a caption sits further off.
const LABEL_REACH: f32 = 4.0;
/// A line whose middle is this close to the page's middle (as a share of the width) is centered.
const CENTER_SLACK: f32 = 0.03;
/// Lines narrower than this share of a full line can show they are centered; full lines are centered trivially.
const CENTER_MAX_WIDTH: f32 = 0.85;
/// Cells whose left edges are this close (as a share of the page width) share a column.
const COLUMN_SLACK: f32 = 0.02;
/// A line counts as code when at least this share of its characters are monospace.
const MONO_SHARE: f32 = 0.8;

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("pattern is valid")
}

static BULLET: LazyLock<Regex> = LazyLock::new(|| re(r"^[•◦▪▸►‣⁃●○■□–»›✓✔➢➤◆◇-]\s*"));
/// A numbered list item: "1. ", "12) ". Longer numbers are years or figures, not items.
static ORDERED: LazyLock<Regex> = LazyLock::new(|| re(r"^(\d{1,3})[.)]\s+"));
/// A run that is only a list marker, set apart from its item text.
static MARKER: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)^(?:[•◦▪▸►‣⁃●○■□–»›✓✔➢➤◆◇-]|\d{1,3}[.)]|\(?[a-z\d]{1,3}\))$"));
/// Figure and table captions.
static CAPTION: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:Figure|Fig\.|Table|Listing|Algorithm|Chart|Exhibit)\s*[\dA-Z]+(?:[.-]\d+)*[.:]\s"));
/// A URL broken at the end of a line, after a character a URL breaks at.
static URL_BREAK: LazyLock<Regex> = LazyLock::new(|| re(r"https?://(?:\S*[/.\-_?=&#~])?$"));
static BOLD_FONT_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)bold|black|heavy|semibold"));
static ITALIC_FONT_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)italic|oblique"));
/// Monospace font names; code in PDFs is set in one of these.
static MONO_FONT_NAME: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)mono|courier|consol|menlo|monaco|inconsolata|typewriter|code|cmtt|txtt|lmtt"));
/// A page label like "xiv | Preface", "Chapter 3 | 57" or a bare "57".
static PAGE_LABEL: LazyLock<Regex> = LazyLock::new(|| {
    let number = r"(?:\d+|m{0,3}(?:cm|cd|d?c{0,3})(?:xc|xl|l?x{0,3})(?:ix|iv|v?i{0,3}))";
    re(&format!(r"(?i)^(?:{number}\s*\|.{{0,60}}|.{{0,60}}\|\s*{number}|{number})$"))
});
/// Line-end hyphens: ASCII, Unicode hyphens, and the "!" some fonts map their hyphen glyph to.
static LINE_END_HYPHEN: LazyLock<Regex> = LazyLock::new(|| re(r"[a-z][-‐!]$"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| re(r"\d+"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| re(r"\s+"));
static LEADERS: LazyLock<Regex> = LazyLock::new(|| re(r"\s*(?:\.\s*){4,}"));
static GAPPED_ENTRY: LazyLock<Regex> = LazyLock::new(|| re(r"\s{3,}(\d+|[ivxlc]+)(?:\s+|$)"));
static TOC_ENTRY: LazyLock<Regex> = LazyLock::new(|| re(r"(?i) · (?:\d+|[ivxlc]+)$"));
static LEADING_SYNTAX: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:[#>+]|\d+[.)]\s)"));

fn round_half(n: f32) -> f32 {
    (n * 2.0).round() / 2.0
}

/// Weights by value, keeping first-seen order so ties go to the earliest value.
struct Tally<K>(Vec<(K, usize)>);

impl<K: PartialEq + Copy> Tally<K> {
    fn new() -> Self {
        Tally(Vec::new())
    }
    fn add(&mut self, key: K, weight: usize) {
        match self.0.iter_mut().find(|(k, _)| *k == key) {
            Some((_, w)) => *w += weight,
            None => self.0.push((key, weight)),
        }
    }
    fn most_common(&self) -> Option<K> {
        let mut best: Option<(K, usize)> = None;
        for &(k, w) in &self.0 {
            if best.is_none_or(|(_, bw)| w > bw) {
                best = Some((k, w));
            }
        }
        best.map(|(k, _)| k)
    }
}

/// Sizes are multiples of half a point, so they compare exactly in half points.
fn half_points(size: f32) -> i32 {
    (size * 2.0).round() as i32
}

fn block_size(lines: &[Line]) -> f32 {
    let mut tally = Tally::new();
    for line in lines {
        tally.add(half_points(line.size), line.text.chars().count());
    }
    tally.most_common().map_or(0.0, |k| k as f32 / 2.0)
}

fn block_text(lines: &[Line]) -> String {
    let joined = lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
    SPACES.replace_all(&joined, " ").trim().to_owned()
}

/// Normalizes page numbers so "Page 3" and "Page 4" count as the same running header.
fn running_key(text: &str) -> String {
    DIGITS.replace_all(text, "#").trim().to_owned()
}

fn starts_with(text: &str, test: impl Fn(char) -> bool) -> bool {
    text.chars().next().is_some_and(test)
}

/// Joins wrapped lines into a paragraph, undoing end-of-line hyphenation and mending URLs.
fn join_lines<S: AsRef<str>>(lines: &[S]) -> String {
    let mut acc = String::new();
    for line in lines {
        let line = line.as_ref();
        let mends_url = URL_BREAK.is_match(&acc) && starts_with(line, |c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if acc.is_empty() || mends_url {
            acc.push_str(line);
        } else if LINE_END_HYPHEN.is_match(&acc) && starts_with(line, |c| c.is_ascii_lowercase()) {
            acc.pop();
            acc.push_str(line);
        } else {
            acc.push(' ');
            acc.push_str(line);
        }
    }
    acc
}

/// Keeps a paragraph that starts like markdown syntax ("10. Title", "# 1")
/// from parsing as a list, heading or quote, with an invisible word joiner.
fn escape_leading(text: &str) -> String {
    if LEADING_SYNTAX.is_match(text) { format!("\u{2060}{text}") } else { text.to_owned() }
}

/// Table-of-contents entries: dot leaders ("Title. . . . . 57") collapse to
/// "Title · 57", and entries set apart by wide gaps ("A      4 B      9") get
/// a line each.
fn collapse_leaders(text: &str) -> String {
    let text = LEADERS.replace_all(text, " · ");
    GAPPED_ENTRY.replace_all(&text, " · $1\n").trim_end().to_owned()
}

fn tidy(text: &str) -> String {
    escape_leading(&collapse_leaders(text))
}

/// A table-of-contents entry ends in its page number; "3. Methods · 41" is not a list item.
fn is_toc_entry(text: &str) -> bool {
    TOC_ENTRY.is_match(&collapse_leaders(text))
}

#[derive(Clone, Copy, PartialEq)]
enum Marker {
    Bullet,
    Ordered,
}

impl Marker {
    fn pattern(self) -> &'static Regex {
        match self {
            Marker::Bullet => &BULLET,
            Marker::Ordered => &ORDERED,
        }
    }
}

/// A paragraph, or a list when it opens with a bullet or a number. Each line
/// that opens with the same kind of marker starts a new item. The flag tells
/// the caller to keep it tight against neighbouring list items.
fn paragraph_markdown(lines: &[Line]) -> (String, bool) {
    let texts: Vec<&str> = lines.iter().map(|l| l.text.trim()).filter(|t| !t.is_empty()).collect();
    let first = texts.first().copied().unwrap_or("");
    let marker = if BULLET.is_match(first) {
        Some(Marker::Bullet)
    } else if ORDERED.is_match(first) && !is_toc_entry(&join_lines(&texts)) {
        Some(Marker::Ordered)
    } else {
        None
    };
    let Some(marker) = marker else {
        return (tidy(&join_lines(&texts)), false);
    };
    let mut items: Vec<Vec<&str>> = Vec::new();
    for text in texts {
        match items.last_mut() {
            Some(item) if !marker.pattern().is_match(text) => item.push(text),
            _ => items.push(vec![text]),
        }
    }
    let md = items
        .iter()
        .map(|item| {
            let text = join_lines(item);
            let rest = tidy(&marker.pattern().replace(&text, ""));
            match marker {
                Marker::Ordered => {
                    let number = ORDERED.captures(&text).and_then(|c| c.get(1)).map_or("1", |m| m.as_str());
                    format!("{number}. {rest}")
                }
                Marker::Bullet => format!("- {rest}"),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    (md, true)
}

/// Code lines as set: line breaks kept, trailing spaces dropped, the shared indent removed.
fn code_text(lines: &[Line]) -> String {
    let texts: Vec<&str> = lines.iter().map(|l| l.text.trim_end()).collect();
    let indent = texts.iter().map(|t| t.chars().count() - t.trim_start().chars().count()).min().unwrap_or(0);
    texts.iter().map(|t| t.chars().skip(indent).collect::<String>()).collect::<Vec<_>>().join("\n")
}

/// A code fence longer than any backtick run inside, so the code can't close it early.
fn fenced(code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(3.max(longest + 1));
    format!("{fence}\n{code}\n{fence}")
}

/// What line spacing is judged against.
#[derive(Clone, Copy)]
struct Spacing {
    body: f32,
    pitch: f32,
}

/// The most common top-to-top distance between consecutive same-size lines.
/// Lines inside paragraphs outnumber paragraph breaks, so this is the line pitch.
fn line_pitch(pages: &[Vec<Block>]) -> f32 {
    let mut deltas = Tally::new();
    for page in pages {
        let lines: Vec<&Line> = page
            .iter()
            .flat_map(|b| match b {
                Block::Text(lines) => lines.iter().collect(),
                _ => Vec::new(),
            })
            .collect();
        for pair in lines.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if !same_size(a.size, b.size) {
                continue;
            }
            let delta = (b.top - a.top).round() as i32;
            if delta > 0 {
                deltas.add(delta, 1);
            }
        }
    }
    deltas.most_common().unwrap_or(0) as f32
}

/// Sizes this close are one size set with a little jitter.
fn same_size(a: f32, b: f32) -> bool {
    (a - b).abs() <= 0.5
}

/// Top-to-top distance of wrapped lines at `size`: the body pitch, scaled up for larger type.
fn pitch_at(spacing: Spacing, size: f32) -> f32 {
    spacing.pitch * if spacing.body > 0.0 { (size / spacing.body).max(1.0) } else { 1.0 }
}

/// Whether `next` sits one line below `prev`, as the next line of the same
/// paragraph. A line starting lowercase continues the sentence, so it may sit
/// a little further away.
fn wraps(prev: &Line, next: &Line, spacing: Spacing) -> bool {
    let continues = starts_with(next.text.trim(), |c| c.is_ascii_lowercase());
    let delta = next.top - prev.top;
    delta > 0.0 && delta <= pitch_at(spacing, next.size) * if continues { 1.6 } else { 1.15 }
}

/// mupdf can put a heading and its body in one block; split where the font size changes.
fn split_by_size(lines: Vec<Line>) -> Vec<Vec<Line>> {
    let mut groups: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        match groups.last_mut() {
            Some(group) if same_size(group[0].size, line.size) => group.push(line),
            _ => groups.push(vec![line]),
        }
    }
    groups
}

/// mupdf can put two paragraphs in one block; split where the gap between lines is wider than a line.
fn split_by_gap(lines: Vec<Line>, spacing: Spacing) -> Vec<Vec<Line>> {
    let mut groups: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        match groups.last_mut() {
            Some(group) if group.last().is_some_and(|prev| line.top <= prev.top || wraps(prev, &line, spacing)) => {
                group.push(line)
            }
            _ => groups.push(vec![line]),
        }
    }
    groups
}

/// Some PDFs put every line in its own text block. Rejoins consecutive blocks
/// with the same size and boldness that sit one line pitch apart, so wrapped
/// paragraphs, list items and two-line headings come back together.
fn merge_wrapped(blocks: Vec<Laid>, spacing: Spacing) -> Vec<Laid> {
    let mut merged: Vec<Laid> = Vec::new();
    for block in blocks {
        if let (Some(Laid::Text(prev)), Laid::Text(lines)) = (merged.last_mut(), &block)
            && let (Some(last), Some(first)) = (prev.last(), lines.first())
        {
            let same_style = same_size(last.size, first.size) && last.bold == first.bold && !last.mono && !first.mono;
            if same_style && wraps(last, first, spacing) {
                prev.extend(lines.iter().cloned());
                continue;
            }
        }
        merged.push(block);
    }
    merged
}

/// Lines on one row, left to right. Lines come in reading order, so a row's lines are adjacent.
fn rows_of(lines: &[Line]) -> Vec<Vec<Line>> {
    let mut rows: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        match rows.last_mut() {
            Some(row) if (line.top - row[0].top).abs() < line.size.min(row[0].size) * 0.5 => row.push(line.clone()),
            _ => rows.push(vec![line.clone()]),
        }
    }
    for row in &mut rows {
        row.sort_by(|a, b| a.left.total_cmp(&b.left));
        // Fake bold prints a run twice, a hair apart; the copy inside the other goes.
        let copies: Vec<usize> = (0..row.len())
            .filter(|&i| {
                let line = &row[i];
                row.iter().enumerate().any(|(j, other)| {
                    j != i
                        && line.left < other.right
                        && other.left < line.right
                        && other.text.trim().len() > line.text.trim().len()
                        && other.text.contains(line.text.trim())
                })
            })
            .collect();
        for i in copies.into_iter().rev() {
            row.remove(i);
        }
    }
    rows
}

/// One row as one line: the runs joined by spaces.
fn join_row(row: &[Line]) -> Line {
    let mut line = row[0].clone();
    if row.len() > 1 {
        line.text = row.iter().map(|l| l.text.trim()).collect::<Vec<_>>().join(" ");
        line.right = row.iter().map(|l| l.right).fold(f32::MIN, f32::max);
        line.bold = row.iter().all(|l| l.bold);
        line.italic = row.iter().all(|l| l.italic);
        line.mono = row.iter().all(|l| l.mono);
    }
    line
}

fn join_rows(lines: &[Line]) -> Vec<Line> {
    rows_of(lines).iter().map(|row| join_row(row)).collect()
}

/// A row of cells: two or more runs, the first not a list marker set apart from its item.
fn is_table_row(row: &[Line]) -> bool {
    row.len() >= 2 && !MARKER.is_match(row[0].text.trim())
}

/// Left edges of the columns the cells line up in, left to right.
fn columns_of<'a>(rows: impl IntoIterator<Item = &'a Vec<Line>>) -> Vec<f32> {
    let mut lefts: Vec<f32> = rows.into_iter().flat_map(|row| row.iter().map(|l| l.left)).collect();
    lefts.sort_by(f32::total_cmp);
    let mut columns: Vec<f32> = Vec::new();
    for left in lefts {
        if columns.last().is_none_or(|last| left - last > COLUMN_SLACK) {
            columns.push(left);
        }
    }
    columns
}

fn cell_text(lines: &[Line]) -> String {
    let texts: Vec<&str> = lines.iter().map(|l| l.text.trim()).collect();
    let text = join_lines(&texts).replace('|', "\\|");
    if !text.is_empty() && lines.iter().all(|l| l.bold) { format!("**{text}**") } else { text }
}

/// Turns rows of cells into a table. A row with nothing in the first column
/// carries on the cells of the row above, wrapped onto a second line.
fn table_of(rows: &[Vec<Line>]) -> Laid {
    let columns = columns_of(rows.iter().filter(|r| is_table_row(r)));
    let column = |line: &Line| columns.iter().rposition(|&left| left <= line.left + COLUMN_SLACK / 2.0).unwrap_or(0);
    let mut cells: Vec<Vec<Vec<Line>>> = Vec::new();
    for row in rows {
        let mut placed: Vec<Vec<Line>> = vec![Vec::new(); columns.len()];
        for line in row {
            placed[column(line)].push(line.clone());
        }
        match cells.last_mut() {
            Some(last) if placed[0].is_empty() => {
                for (into, lines) in last.iter_mut().zip(placed) {
                    into.extend(lines);
                }
            }
            _ => cells.push(placed),
        }
    }
    Laid::Table(cells.iter().map(|row| row.iter().map(|lines| cell_text(lines)).collect()).collect())
}

/// Finds tables: runs of adjacent blocks whose rows hold cells lining up in
/// two or more columns. Rows in every other block join into single lines.
fn tabulate(blocks: Vec<Block>) -> Vec<Laid> {
    let mut out: Vec<Laid> = Vec::new();
    let mut run: Vec<Vec<Line>> = Vec::new();
    let flush = |run: &mut Vec<Vec<Line>>, out: &mut Vec<Laid>| {
        let table_rows = run.iter().filter(|r| is_table_row(r)).count();
        if table_rows >= 2 && columns_of(run.iter().filter(|r| is_table_row(r))).len() >= 2 {
            out.push(table_of(run));
        } else if !run.is_empty() {
            out.push(Laid::Text(run.iter().map(|row| join_row(row)).collect()));
        }
        run.clear();
    };
    for block in blocks {
        let lines = match block {
            Block::Text(lines) => lines,
            Block::Image(id) => {
                flush(&mut run, &mut out);
                out.push(Laid::Image(id));
                continue;
            }
            Block::Note(text) => {
                flush(&mut run, &mut out);
                out.push(Laid::Note(text));
                continue;
            }
        };
        let rows = rows_of(&lines);
        if rows.iter().any(|r| is_table_row(r)) {
            // A table's rows sit close together; a block far below starts afresh.
            let adjacent = match (run.last(), rows.first()) {
                (Some(prev), Some(first)) => first[0].top - prev[0].top < prev[0].size * 4.0,
                _ => false,
            };
            if !adjacent {
                flush(&mut run, &mut out);
            }
            run.extend(rows);
            continue;
        }
        flush(&mut run, &mut out);
        out.push(Laid::Text(rows.iter().map(|row| join_row(row)).collect()));
    }
    flush(&mut run, &mut out);
    out
}

/// Text and images in reading order, with paragraphs split and rejoined.
fn arrange(blocks: Vec<Laid>, spacing: Spacing) -> Vec<Laid> {
    let split = blocks
        .into_iter()
        .flat_map(|block| match block {
            Laid::Text(lines) => split_by_size(lines)
                .into_iter()
                .flat_map(|group| split_by_gap(group, spacing))
                .map(Laid::Text)
                .collect(),
            other => vec![other],
        })
        .collect();
    merge_wrapped(split, spacing)
}

/// A rendered block of markdown, with the lines it came from (none for images, code and tables).
struct Piece {
    md: String,
    list: bool,
    lines: Vec<Line>,
}

impl Piece {
    fn bare(md: String) -> Self {
        Piece { md, list: false, lines: Vec::new() }
    }
}

fn table_markdown(rows: &[Vec<String>]) -> String {
    let row = |cells: &[String]| format!("| {} |", cells.join(" | "));
    let Some((head, body)) = rows.split_first() else { return String::new() };
    let rule: Vec<String> = head.iter().map(|_| "---".to_owned()).collect();
    std::iter::once(row(head)).chain(std::iter::once(row(&rule))).chain(body.iter().map(|r| row(r))).collect::<Vec<_>>().join("\n")
}

/// Centered text: every span's middle is the page's middle, and some span is
/// short enough for that to mean something. Spans are (left, right).
fn is_centered(spans: &[(f32, f32)], measure: f32) -> bool {
    !spans.is_empty()
        && spans.iter().all(|(l, r)| ((l + r) / 2.0 - 0.5).abs() <= CENTER_SLACK)
        && spans.iter().any(|(l, r)| r - l < measure * CENTER_MAX_WIDTH)
}

/// Joins pieces into the page's markdown. List items in a row form one list.
/// Centered text goes in a `<div align="center">` block, which the viewer
/// draws centered; a centered list is judged as a whole, since its items are
/// aligned with each other inside the centered column.
fn join_pieces(pieces: &[Piece], measure: f32) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < pieces.len() {
        let mut end = i + 1;
        if pieces[i].list {
            while end < pieces.len() && pieces[end].list {
                end += 1;
            }
        }
        let run = &pieces[i..end];
        i = end;
        let md = run.iter().map(|p| p.md.as_str()).collect::<Vec<_>>().join("\n");
        let lines: Vec<&Line> = run.iter().flat_map(|p| &p.lines).collect();
        let spans: Vec<(f32, f32)> = if run[0].list {
            let left = lines.iter().map(|l| l.left).fold(f32::MAX, f32::min);
            let right = lines.iter().map(|l| l.right).fold(f32::MIN, f32::max);
            vec![(left, right)]
        } else {
            lines.iter().map(|l| (l.left, l.right)).collect()
        };
        if !lines.is_empty() && is_centered(&spans, measure) {
            out.push(format!("<div align=\"center\">\n{md}\n</div>"));
        } else {
            out.push(md);
        }
    }
    out.join("\n\n")
}

/// A rectangle on the page, in PDF points.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Region {
    fn near(self, other: Region) -> bool {
        self.x0 - FIGURE_GAP <= other.x1
            && other.x0 - FIGURE_GAP <= self.x1
            && self.y0 - FIGURE_GAP <= other.y1
            && other.y0 - FIGURE_GAP <= self.y1
    }

    fn overlaps(self, other: Region) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }

    fn union(self, other: Region) -> Region {
        Region {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn width(self) -> f32 {
        self.x1 - self.x0
    }

    fn height(self) -> f32 {
        self.y1 - self.y0
    }

    /// Where a line sits on a page `width` points wide.
    fn of_line(line: &Line, width: f32) -> Region {
        Region { x0: line.left * width, y0: line.top, x1: line.right * width, y1: line.top + line.size }
    }

    fn holds_middle_of(self, line: Region) -> bool {
        let (x, y) = ((line.x0 + line.x1) / 2.0, (line.y0 + line.y1) / 2.0);
        x >= self.x0 && x <= self.x1 && y >= self.y0 && y <= self.y1
    }
}

/// Vector drawings (diagrams, charts) on a page, to show as images. Shapes
/// that touch or nearly touch group into one drawing. A lone box is a shaded
/// panel and a page-sized one a backdrop, not drawings; a group touching long
/// lines of text is a boxed panel of prose, or icons beside paragraphs.
fn figure_regions(paths: &[Region], lines: &[Line], width: f32, height: f32) -> Vec<Region> {
    let mut groups: Vec<(Region, usize)> = Vec::new();
    for &path in paths {
        let (w, h) = (path.width(), path.height());
        if w < MIN_SHAPE_SIZE || h < MIN_SHAPE_SIZE || w * h > width * height * 0.5 {
            continue;
        }
        let mut group = (path, 1);
        groups.retain(|&(region, shapes)| {
            if region.near(group.0) {
                group = (region.union(group.0), shapes + group.1);
                false
            } else {
                true
            }
        });
        groups.push(group);
    }
    // Arrows and connectors are too thin to be shapes, but one touching two
    // groups joins them into one drawing.
    for &path in paths {
        let thin = path.width() < MIN_SHAPE_SIZE || path.height() < MIN_SHAPE_SIZE;
        if !thin || path.width().max(path.height()) > width * 0.5 {
            continue;
        }
        let touched: Vec<usize> = (0..groups.len()).filter(|&g| groups[g].0.near(path)).collect();
        if let [first, rest @ ..] = touched.as_slice()
            && !rest.is_empty()
        {
            for &g in rest.iter().rev() {
                let (region, shapes) = groups.remove(g);
                groups[*first] = (groups[*first].0.union(region), groups[*first].1 + shapes);
            }
        }
    }
    // Growing a group can bring it near one passed over earlier.
    'merge: loop {
        for i in 0..groups.len() {
            for j in i + 1..groups.len() {
                if groups[i].0.near(groups[j].0) {
                    let (region, shapes) = groups.remove(j);
                    groups[i] = (groups[i].0.union(region), groups[i].1 + shapes);
                    continue 'merge;
                }
            }
        }
        break;
    }
    let placed: Vec<(Region, usize)> = lines.iter().map(|l| (Region::of_line(l, width), l.text.trim().chars().count())).collect();
    groups
        .into_iter()
        .filter(|&(_, shapes)| shapes >= MIN_FIGURE_SHAPES)
        .map(|(r, _)| Region { x0: r.x0 - 2.0, y0: r.y0 - 2.0, x1: r.x1 + 2.0, y1: r.y1 + 2.0 })
        // Labels that poke out of the shapes, or sit just outside them, still belong to the drawing.
        .map(|region| {
            let reach = Region { x0: region.x0 - LABEL_REACH, y0: region.y0 - LABEL_REACH, x1: region.x1 + LABEL_REACH, y1: region.y1 + LABEL_REACH };
            placed
                .iter()
                .filter(|&&(line, len)| len <= MAX_LABEL_LENGTH && line.overlaps(reach))
                .fold(region, |grown, &(line, _)| grown.union(line))
        })
        .filter(|region| region.width() >= MIN_IMAGE_SIZE && region.height() >= MIN_IMAGE_SIZE)
        .filter(|region| !placed.iter().any(|&(line, len)| len > MAX_LABEL_LENGTH && line.overlaps(*region)))
        .collect()
}

/// Learns body size, heading sizes, line pitch, measure and running headers
/// from a sample of pages. Body text is the most common size; larger sizes
/// become headings ranked by size. Text no bigger than body on at least half
/// the sampled pages is a running header or footer.
pub fn analyze(pages: &[Vec<Block>]) -> Layout {
    let text_lines = || {
        pages.iter().flatten().flat_map(|b| match b {
            Block::Text(lines) => lines.as_slice(),
            _ => &[],
        })
    };
    let mut sizes = Tally::new();
    for line in text_lines() {
        sizes.add(half_points(line.size), line.text.chars().count());
    }
    let body = sizes.most_common().map_or(0.0, |k| k as f32 / 2.0);
    let spacing = Spacing { body, pitch: line_pitch(pages) };

    let mut repeats: HashMap<String, usize> = HashMap::new();
    for page in pages {
        let laid = page
            .iter()
            .map(|b| match b {
                Block::Text(lines) => Laid::Text(join_rows(lines)),
                Block::Image(id) => Laid::Image(id.clone()),
                Block::Note(text) => Laid::Note(text.clone()),
            })
            .collect();
        let seen: HashSet<String> = arrange(laid, spacing)
            .iter()
            .filter_map(|b| match b {
                // A title repeated on a cover is not a running header.
                Laid::Text(lines) if block_size(lines) <= body + 0.5 => Some(running_key(&block_text(lines))),
                _ => None,
            })
            .collect();
        for key in seen {
            *repeats.entry(key).or_default() += 1;
        }
    }
    let running = if pages.len() >= 3 {
        repeats
            .into_iter()
            .filter(|(key, count)| key.chars().count() < 80 && *count as f32 >= pages.len() as f32 * 0.5)
            .map(|(key, _)| key)
            .collect()
    } else {
        HashSet::new()
    };

    // Jitter makes one heading size read as two; the larger stands for both.
    let mut candidates: Vec<f32> = sizes.0.iter().map(|&(k, _)| k as f32 / 2.0).filter(|&s| s >= body * 1.15).collect();
    candidates.sort_by(|a, b| b.total_cmp(a));
    let mut heading_sizes: Vec<f32> = Vec::new();
    for size in candidates {
        if !heading_sizes.iter().any(|&s| same_size(s, size)) {
            heading_sizes.push(size);
        }
    }

    let mut widths = Tally::new();
    for line in text_lines() {
        if same_size(line.size, body) {
            widths.add(((line.right - line.left) * 100.0).round() as i32, 1);
        }
    }
    let measure = widths.most_common().map_or(1.0, |w| w as f32 / 100.0);
    Layout { body, heading_sizes, pitch: spacing.pitch, running, measure }
}

/// Turns pages into markdown using a layout learned by `analyze`.
pub fn render(pages: &[Vec<Block>], layout: &Layout) -> String {
    let spacing = Spacing { body: layout.body, pitch: layout.pitch };
    let furniture = |text: &str| text.is_empty() || layout.running.contains(&running_key(text)) || PAGE_LABEL.is_match(text);
    let mut pieces: Vec<Piece> = Vec::new();
    // Back-to-back code blocks gather here and print as one fence.
    let mut code: Vec<String> = Vec::new();
    let flush_code = |code: &mut Vec<String>, pieces: &mut Vec<Piece>| {
        if !code.is_empty() {
            pieces.push(Piece::bare(fenced(&code.join("\n\n"))));
        }
        code.clear();
    };
    for page in pages {
        // Headers and footers go before tables are found, so they don't join one as a row.
        let kept: Vec<Block> = page
            .iter()
            .filter(|b| match b {
                Block::Text(lines) => !furniture(&collapse_leaders(&block_text(&join_rows(lines)))),
                _ => true,
            })
            .cloned()
            .collect();
        for block in arrange(tabulate(kept), spacing) {
            let lines = match block {
                // Code is set no bigger than body text; a large monospace line is a title.
                Laid::Text(lines) if lines.iter().all(|l| l.mono) && block_size(&lines) <= layout.body => {
                    code.push(code_text(&lines));
                    continue;
                }
                Laid::Text(lines) => lines,
                Laid::Image(id) => {
                    flush_code(&mut code, &mut pieces);
                    pieces.push(Piece::bare(format!("![]({})", Doc::image_href(&id))));
                    continue;
                }
                Laid::Note(text) => {
                    flush_code(&mut code, &mut pieces);
                    pieces.push(Piece::bare(format!("> *{text}*")));
                    continue;
                }
                Laid::Table(rows) => {
                    flush_code(&mut code, &mut pieces);
                    pieces.push(Piece::bare(table_markdown(&rows)));
                    continue;
                }
            };
            flush_code(&mut code, &mut pieces);
            let text = collapse_leaders(&block_text(&lines));
            if furniture(&text) {
                continue;
            }
            let size = block_size(&lines);
            let level = layout.heading_sizes.iter().position(|&s| same_size(s, size));
            let length = text.chars().count();
            let piece = if CAPTION.is_match(&text) {
                Piece { md: format!("*{}*", text.replace('*', "\\*")), list: false, lines }
            } else if let Some(level) = level.filter(|_| length < 200 && !BULLET.is_match(&text)) {
                Piece { md: format!("{} {text}", "#".repeat((level + 1).min(3))), list: false, lines }
            } else if lines.iter().all(|l| l.bold) && length < 100 && !BULLET.is_match(&text) && !ORDERED.is_match(&text) {
                Piece { md: format!("#### {text}"), list: false, lines }
            } else {
                let (md, list) = paragraph_markdown(&lines);
                // A paragraph set wholly in italics (a stated question, a pull quote) keeps them.
                let italic = !list && lines.iter().all(|l| l.italic) && !md.contains('*');
                Piece { md: if italic { format!("*{md}*") } else { md }, list, lines }
            };
            pieces.push(piece);
        }
    }
    flush_code(&mut code, &mut pieces);
    join_pieces(&pieces, layout.measure)
}

/// Analyze and render in one step, for documents already fully read.
#[cfg(test)]
pub fn to_markdown(pages: &[Vec<Block>]) -> String {
    render(pages, &analyze(pages))
}

/// Indexes of `count` pages spread evenly across a document of `total` pages.
fn sample_pages(total: usize, count: usize) -> Vec<usize> {
    if total <= count {
        return (0..total).collect();
    }
    (0..count).map(|i| i * total / count).collect()
}

/// An image a page shows: one embedded in the PDF, or a region of the page
/// drawn as vectors. Both are drawn only when shown.
enum PdfImage {
    Embedded { image: mupdf::Image, width: u32, height: u32 },
    Drawing { page: usize, region: Region, width: u32, height: u32 },
}

impl PdfImage {
    fn size(&self) -> (u32, u32) {
        match *self {
            PdfImage::Embedded { width, height, .. } | PdfImage::Drawing { width, height, .. } => (width, height),
        }
    }
}

/// Pixel size an embedded image decodes to, read from its header without decoding.
fn decoded_size(image: &mupdf::Image) -> (u32, u32) {
    let (w, h) = (image.width().max(1), image.height().max(1));
    let scale = (MAX_IMAGE_PIXELS_WIDE as f32 / w as f32).min(1.0);
    (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1))
}

/// Pixel size and scale a page region renders at.
fn region_size(region: Region) -> (f32, u32, u32) {
    let scale = MAX_IMAGE_PIXELS_WIDE as f32 / region.width();
    (scale, MAX_IMAGE_PIXELS_WIDE, ((region.height() * scale).round() as u32).max(1))
}

/// Opaque RGBA pixels of an RGB pixmap with alpha.
fn rgba(pixmap: &Pixmap) -> Option<RgbaImage> {
    let (w, h, n) = (pixmap.width(), pixmap.height(), pixmap.n() as usize);
    let stride = pixmap.stride() as usize;
    let samples = pixmap.samples();
    let mut out = RgbaImage::new(w, h);
    for (y, row) in out.rows_mut().enumerate() {
        for (x, pixel) in row.enumerate() {
            let at = y * stride + x * n;
            let p = samples.get(at..at + 3)?;
            pixel.0 = [p[0], p[1], p[2], 255];
        }
    }
    Some(out)
}

/// Draws an embedded image into a small opaque buffer, on white.
fn decode_embedded(image: &mupdf::Image, width: u32, height: u32) -> Result<Option<RgbaImage>, mupdf::Error> {
    let mut pixmap = Pixmap::new_with_rect(&Colorspace::device_rgb(), IRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 }, true)?;
    pixmap.clear_with(255)?;
    {
        let device = Device::from_pixmap(&pixmap)?;
        let ctm = Matrix::new(width as f32, 0.0, 0.0, height as f32, 0.0, 0.0);
        device.fill_image(image, &ctm, 1.0, ColorParams::default())?;
    }
    Ok(rgba(&pixmap))
}

/// Renders a region of a page, text and all, on white, `width` pixels wide.
fn render_region(page: &Page, region: Region, width: u32) -> Result<Option<RgbaImage>, mupdf::Error> {
    let scale = width as f32 / region.width();
    let height = ((region.height() * scale).round() as u32).max(1);
    let (x, y) = ((region.x0 * scale).round() as i32, (region.y0 * scale).round() as i32);
    let rect = IRect { x0: x, y0: y, x1: x + width as i32, y1: y + height as i32 };
    let mut pixmap = Pixmap::new_with_rect(&Colorspace::device_rgb(), rect, true)?;
    pixmap.clear_with(255)?;
    {
        let device = Device::from_pixmap(&pixmap)?;
        page.run(&device, &Matrix::new_scale(scale, scale))?;
    }
    Ok(rgba(&pixmap))
}

/// Reads one page into blocks, registering the images it shows.
fn read_page(document: &Document, i: usize, images: &RefCell<HashMap<String, PdfImage>>) -> Result<Vec<Block>, mupdf::Error> {
    let page = document.load_page(i as i32)?;
    let bounds = page.bounds()?;
    let (width, height) = ((bounds.x1 - bounds.x0).max(1.0), (bounds.y1 - bounds.y0).max(1.0));
    let text_page = page.to_text_page(TextPageFlags::PRESERVE_IMAGES)?;

    // Each block with its top edge, so drawings can go where they sit on the page.
    enum Raw {
        Text(Vec<Line>),
        Image(mupdf::Image, f32, f32),
    }
    let mut raw: Vec<(f32, Raw)> = Vec::new();
    for block in text_page.blocks() {
        let b = block.bounds();
        match block.r#type() {
            TextBlockType::Text => {
                let mut lines = Vec::new();
                for line in block.lines() {
                    let mut text = String::new();
                    let (mut mono, mut all) = (0usize, 0usize);
                    let mut style: Option<(f32, String, bool, bool)> = None;
                    for c in line.chars() {
                        let Some(ch) = c.char() else { continue };
                        text.push(ch);
                        if ch.is_whitespace() {
                            continue;
                        }
                        all += 1;
                        if let Some(font) = c.font() {
                            if font.is_monospaced() || MONO_FONT_NAME.is_match(font.name()) {
                                mono += 1;
                            }
                            if style.is_none() {
                                style = Some((c.size(), font.name().to_owned(), font.is_bold(), font.is_italic()));
                            }
                        }
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    let (size, name, bold, italic) = style.unwrap_or((0.0, String::new(), false, false));
                    let lb = line.bounds();
                    lines.push(Line {
                        text,
                        size: round_half(size),
                        bold: bold || BOLD_FONT_NAME.is_match(&name),
                        italic: italic || ITALIC_FONT_NAME.is_match(&name),
                        top: lb.y0 - bounds.y0,
                        left: (lb.x0 - bounds.x0) / width,
                        right: (lb.x1 - bounds.x0) / width,
                        mono: all > 0 && mono as f32 >= all as f32 * MONO_SHARE,
                    });
                }
                if let Some(first) = lines.first() {
                    raw.push((first.top, Raw::Text(lines)));
                }
            }
            TextBlockType::Image => {
                if let Some(image) = block.image() {
                    raw.push((b.y0 - bounds.y0, Raw::Image(image, b.x1 - b.x0, b.y1 - b.y0)));
                }
            }
            _ => {}
        }
    }

    // Vector drawings show as images of their part of the page; the labels inside go with them.
    let paths: Vec<Region> = page
        .drawings()?
        .iter()
        .map(|d| Region { x0: d.rect.x0 - bounds.x0, y0: d.rect.y0 - bounds.y0, x1: d.rect.x1 - bounds.x0, y1: d.rect.y1 - bounds.y0 })
        .collect();
    let all_lines: Vec<Line> = raw
        .iter()
        .flat_map(|(_, r)| match r {
            Raw::Text(lines) => lines.clone(),
            Raw::Image(..) => Vec::new(),
        })
        .collect();
    let regions = figure_regions(&paths, &all_lines, width, height);
    let in_figure = |line: &Line| regions.iter().any(|r| r.holds_middle_of(Region::of_line(line, width)));

    let mut placed: Vec<(f32, Block)> = Vec::new();
    let mut images = images.borrow_mut();
    let mut image_index = 0;
    for (top, block) in raw {
        match block {
            Raw::Text(lines) => {
                let lines: Vec<Line> = lines.into_iter().filter(|l| !in_figure(l)).collect();
                if let Some(first) = lines.first() {
                    placed.push((first.top, Block::Text(lines)));
                }
            }
            Raw::Image(image, w, h) => {
                image_index += 1;
                if w < MIN_IMAGE_SIZE || h < MIN_IMAGE_SIZE {
                    continue;
                }
                let id = format!("p{}-{image_index}", i + 1);
                let (width, height) = decoded_size(&image);
                images.insert(id.clone(), PdfImage::Embedded { image, width, height });
                placed.push((top, Block::Image(id)));
            }
        }
    }
    for (n, region) in regions.into_iter().enumerate() {
        let id = format!("p{}-figure-{}", i + 1, n + 1);
        let (_, width, height) = region_size(region);
        images.insert(id.clone(), PdfImage::Drawing { page: i, region, width, height });
        let at = placed.iter().position(|(top, _)| *top >= region.y0).unwrap_or(placed.len());
        placed.insert(at, (region.y0, Block::Image(id)));
    }
    Ok(placed.into_iter().map(|(_, b)| b).collect())
}

/// One malformed page must not lose the whole document: note it and keep going.
fn safe_read_page(document: &Document, i: usize, images: &RefCell<HashMap<String, PdfImage>>) -> Vec<Block> {
    read_page(document, i, images).unwrap_or_else(|e| {
        vec![Block::Note(format!("Page {} could not be read ({e}). The other pages are unaffected.", i + 1))]
    })
}

struct PdfSource {
    document: Document,
    layout: Layout,
    /// Markdown is small next to mupdf's page structures, so each page is read once and kept.
    pages: RefCell<HashMap<usize, Rc<str>>>,
    images: RefCell<HashMap<String, PdfImage>>,
}

impl Source for PdfSource {
    fn page(&self, index: usize) -> Rc<str> {
        if let Some(page) = self.pages.borrow().get(&index) {
            return page.clone();
        }
        let blocks = safe_read_page(&self.document, index, &self.images);
        let page: Rc<str> = render(&[blocks], &self.layout).into();
        self.pages.borrow_mut().insert(index, page.clone());
        page
    }

    fn image_size(&self, id: &str) -> Option<(u32, u32)> {
        self.images.borrow().get(id).map(PdfImage::size)
    }

    // Decoded each time it's shown rather than kept: a book's worth of pixels doesn't fit in memory.
    // An image mupdf cannot decode is left out; the text around it still shows.
    // Drawings render sharp at the width they're shown; embedded images never beyond their own pixels.
    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage> {
        let width = width.clamp(1, MAX_RENDER_PIXELS_WIDE);
        let images = self.images.borrow();
        match images.get(id)? {
            PdfImage::Embedded { image, .. } => {
                let (w, h) = (image.width().max(1), image.height().max(1));
                let scale = (width as f32 / w as f32).min(1.0);
                let size = (((w as f32 * scale).round() as u32).max(1), ((h as f32 * scale).round() as u32).max(1));
                decode_embedded(image, size.0, size.1).ok().flatten()
            }
            PdfImage::Drawing { page, region, .. } => {
                let page = self.document.load_page(*page as i32).ok()?;
                render_region(&page, *region, width).ok().flatten()
            }
        }
    }
}

/// Opens a PDF and returns at once. Only a small sample of pages is read up
/// front, to learn the layout; the rest are read as they are shown.
pub fn open(path: &Path) -> Result<Doc, PdfOpenFailed> {
    let bytes = std::fs::read(path).map_err(|e| PdfOpenFailed {
        message: if e.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "macOS blocked access to {} ({e}). Allow your terminal in System Settings → Privacy & Security → Files & Folders, then restart it.",
                path.display()
            )
        } else {
            format!("Could not read {} ({e}). Check that the path is right and the file exists.", path.display())
        },
    })?;
    let fallback = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    from_bytes(&bytes, &path.display().to_string(), &fallback)
}

/// `open` for PDF bytes already in hand (a download, say). `source` names the
/// PDF in errors; `fallback_title` is used when the PDF has no title of its own.
pub fn from_bytes(bytes: &[u8], source: &str, fallback_title: &str) -> Result<Doc, PdfOpenFailed> {
    let failed = |e: mupdf::Error| PdfOpenFailed {
        message: format!(
            "Could not open {source} as a PDF ({e}). It may be encrypted or damaged. Open it in a PDF viewer to confirm it is valid."
        ),
    };
    let document = Document::from_bytes(bytes, "application/pdf").map_err(failed)?;
    let page_count = document.page_count().map_err(failed)?.max(0) as usize;
    let images = RefCell::new(HashMap::new());
    let sample: Vec<Vec<Block>> = sample_pages(page_count, SAMPLE_PAGES).into_iter().map(|i| safe_read_page(&document, i, &images)).collect();
    let layout = analyze(&sample);
    let title = document.metadata(MetadataName::Title).ok().map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
    Ok(Doc {
        title: title.unwrap_or_else(|| fallback_title.to_owned()),
        page_count: Some(page_count),
        source: Box::new(PdfSource { document, layout, pages: RefCell::new(HashMap::new()), images }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, top: f32) -> Line {
        Line { text: text.into(), size: 11.0, bold: false, italic: false, top, left: 0.1, right: 0.9, mono: false }
    }
    fn sized(text: &str, top: f32, size: f32, bold: bool) -> Line {
        Line { size, bold, ..line(text, top) }
    }
    fn placed(text: &str, top: f32, left: f32, right: f32) -> Line {
        Line { left, right, ..line(text, top) }
    }
    fn code(text: &str, top: f32) -> Line {
        Line { size: 9.0, mono: true, ..line(text, top) }
    }
    fn text(lines: Vec<Line>) -> Block {
        Block::Text(lines)
    }
    const BODY: &str = "Body text long enough to set the common size for the whole page here.";
    fn prose(top: f32) -> Block {
        text(vec![line(BODY, top)])
    }
    /// Four lines of body text at a 12pt pitch, so the page has a line pitch to judge gaps by.
    fn body(top: f32) -> Block {
        text((0..4).map(|i| line(BODY, top + i as f32 * 12.0)).collect())
    }
    fn md(page: Vec<Block>) -> String {
        to_markdown(&[page])
    }

    #[test]
    fn larger_sizes_become_headings_ranked_by_size() {
        let out = md(vec![
            text(vec![sized("Big Title", 10.0, 24.0, false)]),
            text(vec![sized("Section", 50.0, 16.0, false)]),
            text(vec![line("Body text that is long enough to be the most common size.", 80.0)]),
            text(vec![line("More body text so body clearly dominates the weights.", 120.0)]),
        ]);
        assert!(out.starts_with("# Big Title\n\n## Section\n\n"), "{out}");
    }

    #[test]
    fn one_line_blocks_a_pitch_apart_rejoin_undoing_hyphenation() {
        let out = md(vec![
            text(vec![line("The quick brown fox jumps over the lazy dog and keeps run-", 0.0)]),
            text(vec![line("ning until the end.", 20.0)]),
            text(vec![line("A new paragraph starts after a bigger gap here.", 50.0)]),
            text(vec![line("It wraps onto a second line at the normal pitch.", 70.0)]),
        ]);
        assert_eq!(
            out,
            "The quick brown fox jumps over the lazy dog and keeps running until the end.\n\n\
             A new paragraph starts after a bigger gap here. It wraps onto a second line at the normal pitch."
        );
    }

    #[test]
    fn a_bold_line_is_not_merged_into_body_below() {
        let out = md(vec![
            text(vec![sized("Part 1: The Crash", 0.0, 11.0, true)]),
            text(vec![line("The phone didn't just ring; it screamed loudly at night.", 20.0)]),
            text(vec![line("Another line of body copy at the same pitch as before.", 40.0)]),
        ]);
        assert!(out.starts_with("#### Part 1: The Crash\n\n"), "{out}");
    }

    #[test]
    fn bullet_glyphs_become_list_items() {
        let out = md(vec![text(vec![line("●First item", 0.0), line("●Second item", 20.0), line("continues here", 40.0)])]);
        assert_eq!(out, "- First item\n- Second item continues here");
    }

    #[test]
    fn text_repeated_on_most_pages_is_a_running_header() {
        let pages: Vec<Vec<Block>> = ["Alpha starts the story.", "Bravo raises the stakes.", "Charlie turns it.", "Delta ends it."]
            .iter()
            .enumerate()
            .map(|(i, b)| {
                vec![
                    text(vec![line(&format!("Page {}", i + 1), 0.0)]),
                    text(vec![line(b, 100.0), line("It wraps onto a second line.", 120.0), line("And a third one.", 140.0)]),
                ]
            })
            .collect();
        let out = to_markdown(&pages);
        assert!(!out.contains("Page "), "{out}");
        assert!(out.contains("Charlie turns it."));
    }

    #[test]
    fn image_blocks_become_image_references() {
        assert_eq!(md(vec![Block::Image("p1-0".into())]), "![](leaf-image:p1-0)");
    }

    #[test]
    fn page_labels_and_bare_numbers_are_dropped() {
        let out = md(vec![
            text(vec![line("Body text that is long enough to be the most common size here.", 0.0)]),
            text(vec![sized("xiv | Preface", 100.0, 9.0, false)]),
            text(vec![sized("Table of Contents | ix", 120.0, 9.0, false)]),
            text(vec![sized("57", 140.0, 9.0, false)]),
        ]);
        assert_eq!(out, "Body text that is long enough to be the most common size here.");
    }

    #[test]
    fn a_hyphen_glyph_mapped_to_bang_is_undone() {
        assert_eq!(md(vec![text(vec![line("bombarded with buzz!", 0.0), line("words relating to storage.", 20.0)])]), "bombarded with buzzwords relating to storage.");
    }

    #[test]
    fn toc_leaders_collapse_and_chapter_numbers_stay_escaped() {
        assert_eq!(md(vec![text(vec![line("10. Batch Processing. . . . . . . . . . . . . .  389", 0.0)])]), "\u{2060}10. Batch Processing · 389");
    }

    #[test]
    fn toc_entries_set_apart_by_wide_gaps_get_a_line_each() {
        assert_eq!(
            md(vec![text(vec![line("Total Order Broadcast        348 Distributed Transactions        352", 0.0)])]),
            "Total Order Broadcast · 348\nDistributed Transactions · 352"
        );
    }

    #[test]
    fn monospace_blocks_become_fences_keeping_lines_and_indentation() {
        let out = md(vec![
            prose(0.0),
            text(vec![code("for (var i = 0; i < n; i++) {", 20.0), code("    if (x) {", 30.0), code("        go()", 40.0), code("    }", 50.0), code("}", 60.0)]),
            prose(80.0),
        ]);
        assert!(out.contains("```\nfor (var i = 0; i < n; i++) {\n    if (x) {\n        go()\n    }\n}\n```"), "{out}");
    }

    #[test]
    fn shared_indent_is_dedented_and_backtick_runs_lengthen_the_fence() {
        let out = md(vec![prose(0.0), text(vec![code("  <ul>  ", 20.0), code("    <li>", 30.0), code("  </ul>", 40.0)])]);
        assert!(out.contains("```\n<ul>\n  <li>\n</ul>\n```"), "{out}");
        assert!(md(vec![prose(0.0), text(vec![code("```js", 20.0)])]).contains("````\n```js\n````"));
    }

    #[test]
    fn back_to_back_code_shares_a_fence_and_never_merges_into_prose() {
        let out = md(vec![prose(0.0), text(vec![code("a = 1", 20.0)]), text(vec![code("b = 2", 50.0)])]);
        assert!(out.contains("```\na = 1\n\nb = 2\n```"), "{out}");
        let out = md(vec![prose(0.0), text(vec![code("SELECT 1", 12.0)]), text(vec![line("then prose continues right below the code.", 24.0)])]);
        assert!(out.contains("```\n\nthen prose continues"), "{out}");
    }

    #[test]
    fn sizes_half_a_point_apart_stay_one_paragraph() {
        let out = md(vec![text(vec![sized("A paragraph whose font size jitters from", 0.0, 10.0, false), sized("line to line, as some PDF tools leave it.", 12.0, 10.5, false)])]);
        assert_eq!(out, "A paragraph whose font size jitters from line to line, as some PDF tools leave it.");
    }

    #[test]
    fn a_gap_wider_than_a_line_inside_a_block_starts_a_paragraph() {
        let out = md(vec![text(vec![line("The first paragraph runs over two", 0.0), line("lines and then ends here.", 20.0), line("A second paragraph starts after a gap.", 45.0)])]);
        assert_eq!(out, "The first paragraph runs over two lines and then ends here.\n\nA second paragraph starts after a gap.");
    }

    #[test]
    fn a_heading_wrapped_onto_two_lines_comes_back_as_one() {
        let out = md(vec![
            text(vec![sized("A Long Title That Wraps Onto", 0.0, 20.0, true)]),
            text(vec![sized("A Second Line", 22.0, 20.0, true)]),
            prose(60.0),
            prose(80.0),
        ]);
        assert!(out.starts_with("# A Long Title That Wraps Onto A Second Line\n\n"), "{out}");
    }

    #[test]
    fn cells_in_columns_become_a_table_with_wrapped_cells_joined() {
        let cell = |t: &str, top: f32, left: f32, bold: bool| Line { size: 8.0, bold, left, right: left + 0.2, ..line(t, top) };
        let out = md(vec![
            prose(0.0),
            text(vec![cell("Name", 30.0, 0.1, true), cell("Purpose", 30.0, 0.4, false)]),
            text(vec![
                cell("Gateway", 41.0, 0.1, true),
                cell("Takes requests and", 41.0, 0.4, false),
                cell("returns results", 52.0, 0.4, false),
                cell("Queue", 63.0, 0.1, true),
                cell("Holds | work", 63.0, 0.4, false),
            ]),
            prose(90.0),
        ]);
        assert!(
            out.contains("| **Name** | Purpose |\n| --- | --- |\n| **Gateway** | Takes requests and returns results |\n| **Queue** | Holds \\| work |"),
            "{out}"
        );
    }

    #[test]
    fn numbered_items_become_one_ordered_list_keeping_numbers() {
        let out = md(vec![
            body(0.0),
            text(vec![line("3. Registry: keeps track of every worker", 70.0), placed("and its health.", 82.0, 0.12, 0.9)]),
            text(vec![line("4. Scheduler: picks a worker.", 100.0)]),
        ]);
        assert!(out.contains("\n\n3. Registry: keeps track of every worker and its health.\n4. Scheduler: picks a worker."), "{out}");
    }

    #[test]
    fn a_list_number_set_apart_from_its_item_joins_it() {
        let item = |n: u32, name: &str, top: f32| text(vec![placed(&format!("{n}."), top, 0.42, 0.43), placed(name, top, 0.45, 0.56)]);
        let out = md(vec![body(0.0), item(1, "Ada Lovelace", 70.0), item(2, "Alan Turing", 90.0)]);
        assert!(out.contains("1. Ada Lovelace\n2. Alan Turing"), "{out}");
        assert!(!out.contains('|'));
    }

    #[test]
    fn bullet_items_in_separate_blocks_form_one_tight_list() {
        let out = md(vec![body(0.0), text(vec![line("• First", 70.0)]), text(vec![line("» Second", 90.0)])]);
        assert!(out.contains("- First\n- Second"), "{out}");
    }

    #[test]
    fn centered_lines_are_marked_full_and_left_lines_are_not() {
        let out = md(vec![text(vec![placed("Submitted By", 0.0, 0.44, 0.56)]), body(30.0), text(vec![placed("A short left-aligned line.", 100.0, 0.1, 0.4)])]);
        assert!(out.starts_with("<div align=\"center\">\nSubmitted By\n</div>\n\nBody text"), "{out}");
        assert!(out.ends_with("whole page here.\n\nA short left-aligned line."), "{out}");
    }

    #[test]
    fn captions_are_italic_not_headings() {
        let out = md(vec![prose(0.0), text(vec![sized("Figure 1: Logical architecture.", 30.0, 8.0, true)])]);
        assert!(out.contains("*Figure 1: Logical architecture.*"), "{out}");
        assert!(!out.contains('#'));
    }

    #[test]
    fn a_url_broken_across_lines_is_mended() {
        assert_eq!(md(vec![text(vec![line("See https://www.usenix.org/", 0.0), line("conference/osdi22 for details.", 12.0)])]), "See https://www.usenix.org/conference/osdi22 for details.");
    }

    #[test]
    fn a_large_title_on_cover_and_first_page_is_not_a_running_header() {
        let pages: Vec<Vec<Block>> = ["Alpha starts the story.", "Bravo raises the stakes.", "Charlie turns it.", "Delta ends it."]
            .iter()
            .enumerate()
            .map(|(i, b)| {
                let mut page = if i < 2 { vec![text(vec![sized("The Title Of The Work", 0.0, 20.0, true)])] } else { vec![] };
                page.push(text(vec![line(b, 100.0), line("It wraps onto a second line.", 120.0)]));
                page
            })
            .collect();
        assert!(to_markdown(&pages).contains("# The Title Of The Work"));
    }

    #[test]
    fn a_paragraph_wholly_in_italics_keeps_them() {
        let out = md(vec![body(0.0), text(vec![Line { italic: true, ..line("How can requests be scheduled well?", 70.0) }])]);
        assert!(out.contains("*How can requests be scheduled well?*"), "{out}");
    }

    #[test]
    fn a_url_broken_right_after_the_scheme_is_mended() {
        assert_eq!(md(vec![text(vec![line("visit https://", 0.0), line("redislabs.com.", 12.0)])]), "visit https://redislabs.com.");
    }

    #[test]
    fn a_fake_bold_double_print_is_one_list_item_not_a_table_or_heading() {
        let item = |t: &str, top: f32| {
            let run = Line { size: 14.0, bold: true, ..placed(&format!("» {t}"), top, 0.2, 0.3) };
            text(vec![Line { text: "»".into(), left: 0.21, right: 0.22, ..run.clone() }, run])
        };
        let out = md(vec![body(0.0), item("Strings", 70.0), item("Lists", 90.0)]);
        assert!(out.contains("- Strings
- Lists"), "{out}");
    }
}
