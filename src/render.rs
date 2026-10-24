//! Lays markdown out into rows of styled text for a given column width.
//!
//! Every piece of readable text is also appended to the page's plain text,
//! and each drawn segment records where its characters sit in it. Selection
//! and highlights work in those offsets, so they survive a change of width.

use std::sync::LazyLock;

use pulldown_cmark::{
    BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use ratatui::style::{Color, Modifier, Style};
use regex::Regex;
use unicode_width::UnicodeWidthChar;

use crate::font;
use crate::syntax;

/// Tallest an image gets, in rows, so one figure never fills several screens.
const MAX_IMAGE_ROWS: u16 = 28;
/// Rows held for a remote image whose size isn't known yet.
const REMOTE_IMAGE_ROWS: u16 = 12;

pub const DIM: Color = Color::Indexed(8);
pub const CODE: Color = Color::Indexed(3);

/// A block of centered lines, as PDF pages mark title pages and captions.
static CENTERED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^<div align="center">\n(.*?)\n</div>\s*$"#).expect("pattern is valid")
});
static HEADING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6}) (.+)$").expect("pattern is valid"));
static HTML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"</?[A-Za-z][^>]*>").expect("pattern is valid"));
static LINE_BREAK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^<br\s*/?>$").expect("pattern is valid"));

/// The markdown leaf reads: CommonMark with GitHub's extensions (tables,
/// task lists, strikethrough, footnotes, alerts), definition lists and math.
pub const MARKDOWN: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_GFM)
    .union(Options::ENABLE_DEFINITION_LIST)
    .union(Options::ENABLE_MATH);

/// A run of same-styled text at column `x`. `offset` is where its first
/// character sits in the page text; decorations (list markers, borders,
/// large heading letters) have none and can't be selected.
#[derive(Debug, Clone, PartialEq)]
pub struct Seg {
    pub x: u16,
    pub text: String,
    pub style: Style,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub segs: Vec<Seg>,
}

/// Space held for an image: `rows` rows from row `top`, `cols` wide at column `x`.
#[derive(Debug, Clone, PartialEq)]
pub struct Figure {
    pub href: String,
    pub top: usize,
    pub x: u16,
    pub cols: u16,
    pub rows: u16,
}

/// A page laid out at one width.
#[derive(Debug, Default)]
pub struct Laid {
    pub rows: Vec<Row>,
    pub figures: Vec<Figure>,
    /// The page's readable text: each block's text followed by a newline.
    pub text: Vec<char>,
}

/// Rough pixels per terminal column, so small images aren't blown up to full width.
const PIXELS_PER_COLUMN: u32 = 8;

/// Terminal cell size for an image, given the column budget, the image's
/// pixel size and a cell's pixel size (for its shape).
pub fn image_cells(max_cols: u16, width: u32, height: u32, cell: (u16, u16)) -> (u16, u16) {
    let (width, height) = (width.max(1), height.max(1));
    let (cell_w, cell_h) = (cell.0.max(1) as u32, cell.1.max(1) as u32);
    let cols = (width.div_ceil(PIXELS_PER_COLUMN) as u16).clamp(1, max_cols.max(1));
    let rows =
        ((cols as f32 * cell_w as f32 * height as f32 / width as f32 / cell_h as f32).round()
            as u16)
            .clamp(1, MAX_IMAGE_ROWS);
    (cols, rows)
}

// ---------------------------------------------------------------------------
// Markdown into a small tree.

#[derive(Debug, Clone)]
struct Run {
    text: String,
    style: Style,
}

type Inlines = Vec<Run>;

#[derive(Debug, Clone)]
enum Node {
    Paragraph(Inlines),
    Heading(u8, Inlines),
    /// A block quote, or a GitHub alert (`> [!NOTE]`) when it has a kind.
    Quote(Option<BlockQuoteKind>, Vec<Node>),
    Code {
        lang: Option<String>,
        text: String,
    },
    List {
        start: Option<u64>,
        items: Vec<Vec<Node>>,
        tight: bool,
    },
    Table {
        head: Vec<Inlines>,
        rows: Vec<Vec<Inlines>>,
    },
    Figures(Vec<String>),
    Centered(String),
    Footnote(String, Vec<Node>),
    /// Terms (bold paragraphs) and their definitions.
    Definitions(Vec<Node>),
    Definition(Vec<Node>),
    Rule,
}

/// Images in a paragraph of nothing else (but whitespace) show as figures.
const IMAGE_MARK: char = '\u{FFFC}';

struct Builder {
    /// Open containers, innermost last; each collects the blocks inside it.
    stack: Vec<Frame>,
    /// Inline styles in force.
    styles: Vec<Style>,
    inlines: Inlines,
    images: Vec<String>,
    links: Vec<String>,
    code: Option<(Option<String>, String)>,
    html: Option<String>,
    table: Option<TableParts>,
    /// Whether HTML blocks are converted to markdown and read; off while reading a converted block.
    read_html: bool,
}

/// A table being read: its head, the rows so far, and the cells of the row in progress.
#[derive(Default)]
struct TableParts {
    head: Vec<Inlines>,
    rows: Vec<Vec<Inlines>>,
    row: Vec<Inlines>,
}

enum Frame {
    Root(Vec<Node>),
    Quote(Option<BlockQuoteKind>, Vec<Node>),
    List {
        start: Option<u64>,
        items: Vec<Vec<Node>>,
        tight: bool,
    },
    Item(Vec<Node>),
    Footnote(String, Vec<Node>),
    Definitions(Vec<Node>),
    Definition(Vec<Node>),
}

impl Frame {
    fn blocks(&mut self) -> &mut Vec<Node> {
        match self {
            Frame::Root(b)
            | Frame::Quote(_, b)
            | Frame::Item(b)
            | Frame::Footnote(_, b)
            | Frame::Definitions(b)
            | Frame::Definition(b) => b,
            Frame::List { items, .. } => {
                if items.is_empty() {
                    items.push(Vec::new());
                }
                items.last_mut().expect("an item exists")
            }
        }
    }
}

impl Builder {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        self.styles.push(f(self.style()));
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        match self.inlines.last_mut() {
            Some(run) if run.style == style => run.text.push_str(text),
            _ => self.inlines.push(Run {
                text: text.to_owned(),
                style,
            }),
        }
    }

    fn emit(&mut self, node: Node) {
        if let Some(frame) = self.stack.last_mut() {
            frame.blocks().push(node);
        }
    }

    /// Ends the inline text gathered so far as a paragraph (or figures).
    fn flush_paragraph(&mut self) {
        let inlines = std::mem::take(&mut self.inlines);
        let images = std::mem::take(&mut self.images);
        let only_images = !images.is_empty()
            && inlines
                .iter()
                .all(|r| r.text.chars().all(|c| c == IMAGE_MARK || c.is_whitespace()));
        if only_images {
            self.emit(Node::Figures(images));
        } else if inlines.iter().any(|r| !r.text.trim().is_empty()) {
            let inlines = inlines
                .into_iter()
                .map(|r| Run {
                    text: r.text.replace(IMAGE_MARK, ""),
                    style: r.style,
                })
                .collect();
            self.emit(Node::Paragraph(inlines));
        }
    }
}

/// The fence's language, when it names one. An unknown name is kept so a
/// guess doesn't replace it. PDF and EPUB fences have no language.
fn fence_lang(info: &str) -> Option<String> {
    let name = info
        .split(|c: char| c.is_whitespace() || c == ',')
        .find(|part| !part.is_empty())?;
    Some(name.to_owned())
}

fn code_style(kind: syntax::Kind) -> Style {
    match kind {
        syntax::Kind::Plain => Style::default(),
        syntax::Kind::Comment => Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
        syntax::Kind::Keyword => Style::default()
            .fg(Color::Indexed(5))
            .add_modifier(Modifier::BOLD),
        syntax::Kind::String | syntax::Kind::Added => Style::default().fg(Color::Indexed(2)),
        syntax::Kind::Number | syntax::Kind::Type | syntax::Kind::Attribute => {
            Style::default().fg(Color::Indexed(6))
        }
        syntax::Kind::Special => Style::default()
            .fg(Color::Indexed(6))
            .add_modifier(Modifier::ITALIC),
        syntax::Kind::Function => Style::default().fg(Color::Indexed(4)),
        syntax::Kind::Constant => Style::default().fg(Color::Indexed(3)),
        syntax::Kind::Tag => Style::default().fg(Color::Indexed(5)),
        syntax::Kind::Punct => Style::default().fg(DIM),
        syntax::Kind::Removed => Style::default().fg(Color::Indexed(1)),
    }
}

fn parse(markdown: &str, read_html: bool) -> Vec<Node> {
    let mut b = Builder {
        stack: vec![Frame::Root(Vec::new())],
        styles: Vec::new(),
        inlines: Vec::new(),
        images: Vec::new(),
        links: Vec::new(),
        code: None,
        html: None,
        table: None,
        read_html,
    };
    let mut image_depth = 0usize;
    for event in Parser::new_ext(markdown, MARKDOWN) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.flush_paragraph(),
                Tag::Heading { .. } => b.flush_paragraph(),
                Tag::BlockQuote(kind) => {
                    b.flush_paragraph();
                    b.stack.push(Frame::Quote(kind, Vec::new()));
                }
                Tag::FootnoteDefinition(label) => {
                    b.flush_paragraph();
                    b.stack.push(Frame::Footnote(label.to_string(), Vec::new()));
                }
                Tag::DefinitionList => {
                    b.flush_paragraph();
                    b.stack.push(Frame::Definitions(Vec::new()));
                }
                Tag::DefinitionListTitle => b.flush_paragraph(),
                Tag::DefinitionListDefinition => {
                    b.flush_paragraph();
                    b.stack.push(Frame::Definition(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush_paragraph();
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => fence_lang(&info),
                        CodeBlockKind::Indented => None,
                    };
                    b.code = Some((lang, String::new()));
                }
                Tag::HtmlBlock => {
                    b.flush_paragraph();
                    b.html = Some(String::new());
                }
                Tag::List(start) => {
                    b.flush_paragraph();
                    b.stack.push(Frame::List {
                        start,
                        items: Vec::new(),
                        tight: true,
                    });
                }
                Tag::Item => {
                    b.flush_paragraph();
                    b.stack.push(Frame::Item(Vec::new()));
                }
                Tag::Table(_) => {
                    b.flush_paragraph();
                    b.table = Some(TableParts::default());
                }
                Tag::TableHead | Tag::TableRow => {}
                Tag::TableCell => b.inlines.clear(),
                Tag::Emphasis => b.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strong => b.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => {
                    b.push_style(|s| s.add_modifier(Modifier::DIM | Modifier::CROSSED_OUT))
                }
                Tag::Link { dest_url, .. } => {
                    b.links.push(dest_url.to_string());
                    b.push_style(|s| s.add_modifier(Modifier::UNDERLINED));
                }
                Tag::Image { dest_url, .. } => {
                    image_depth += 1;
                    b.images.push(dest_url.to_string());
                    b.text(&IMAGE_MARK.to_string());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    // In a loose list, items hold paragraphs.
                    if let Some(Frame::Item(_)) = b.stack.last()
                        && let Some(Frame::List { tight, .. }) = b.stack.iter_mut().rev().nth(1)
                    {
                        *tight = false;
                    }
                    b.flush_paragraph();
                }
                TagEnd::Heading(level) => {
                    let depth = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    let inlines = std::mem::take(&mut b.inlines);
                    b.images.clear();
                    b.emit(Node::Heading(depth, inlines));
                }
                TagEnd::BlockQuote(_) => {
                    b.flush_paragraph();
                    if let Some(Frame::Quote(kind, blocks)) = b.stack.pop() {
                        b.emit(Node::Quote(kind, blocks));
                    }
                }
                TagEnd::FootnoteDefinition => {
                    b.flush_paragraph();
                    if let Some(Frame::Footnote(label, blocks)) = b.stack.pop() {
                        b.emit(Node::Footnote(label, blocks));
                    }
                }
                TagEnd::DefinitionList => {
                    b.flush_paragraph();
                    if let Some(Frame::Definitions(blocks)) = b.stack.pop() {
                        b.emit(Node::Definitions(blocks));
                    }
                }
                TagEnd::DefinitionListTitle => {
                    for run in &mut b.inlines {
                        run.style = run.style.add_modifier(Modifier::BOLD);
                    }
                    b.flush_paragraph();
                }
                TagEnd::DefinitionListDefinition => {
                    b.flush_paragraph();
                    if let Some(Frame::Definition(blocks)) = b.stack.pop() {
                        b.emit(Node::Definition(blocks));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = b.code.take() {
                        let text = code.strip_suffix('\n').unwrap_or(&code).to_owned();
                        b.emit(Node::Code { lang, text });
                    }
                }
                TagEnd::HtmlBlock => {
                    let Some(html) = b.html.take() else { continue };
                    match CENTERED.captures(&html).and_then(|c| c.get(1)) {
                        Some(inner) if !HTML_TAG.is_match(inner.as_str()) => {
                            b.emit(Node::Centered(inner.as_str().to_owned()))
                        }
                        // Other HTML (a README's centered logo, <details>) is read as the markdown it stands for.
                        _ if b.read_html => {
                            for node in htmd::convert(&html)
                                .map(|md| parse(&md, false))
                                .unwrap_or_default()
                            {
                                b.emit(node);
                            }
                        }
                        _ => {}
                    }
                }
                TagEnd::List(_) => {
                    b.flush_paragraph();
                    if let Some(Frame::List {
                        start,
                        items,
                        tight,
                    }) = b.stack.pop()
                    {
                        b.emit(Node::List {
                            start,
                            items,
                            tight,
                        });
                    }
                }
                TagEnd::Item => {
                    b.flush_paragraph();
                    if let Some(Frame::Item(blocks)) = b.stack.pop()
                        && let Some(Frame::List { items, .. }) = b.stack.last_mut()
                    {
                        items.push(blocks);
                    }
                }
                TagEnd::TableCell => {
                    let cell = std::mem::take(&mut b.inlines);
                    if let Some(table) = b.table.as_mut() {
                        table.row.push(cell);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = b.table.as_mut() {
                        table.head = std::mem::take(&mut table.row);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = b.table.as_mut() {
                        table.rows.push(std::mem::take(&mut table.row));
                    }
                }
                TagEnd::Table => {
                    if let Some(TableParts { head, rows, .. }) = b.table.take() {
                        b.emit(Node::Table { head, rows });
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    b.styles.pop();
                }
                TagEnd::Link => {
                    b.styles.pop();
                    if let Some(url) = b.links.pop() {
                        let label: String =
                            b.inlines.last().map(|r| r.text.clone()).unwrap_or_default();
                        if !label.ends_with(&url) && !url.starts_with('#') {
                            b.push_style(|s| s.fg(DIM).remove_modifier(Modifier::UNDERLINED));
                            b.text(&format!(" ({url})"));
                            b.styles.pop();
                        }
                    }
                }
                TagEnd::Image => image_depth = image_depth.saturating_sub(1),
                _ => {}
            },
            Event::Text(text) => {
                if let Some((_, code)) = b.code.as_mut() {
                    code.push_str(&text);
                } else if image_depth == 0 {
                    b.text(&text);
                }
            }
            Event::Code(code) => {
                b.push_style(|s| s.fg(CODE));
                b.text(&code);
                b.styles.pop();
            }
            Event::Html(html) => {
                if let Some(block) = b.html.as_mut() {
                    block.push_str(&html);
                }
            }
            Event::InlineHtml(html) => {
                if LINE_BREAK.is_match(&html) {
                    b.text("\n");
                }
            }
            Event::FootnoteReference(label) => {
                b.push_style(|s| s.fg(DIM));
                b.text(&format!("[{label}]"));
                b.styles.pop();
            }
            Event::InlineMath(math) => {
                b.push_style(|s| s.fg(CODE));
                b.text(&format!("${math}$"));
                b.styles.pop();
            }
            Event::DisplayMath(math) => {
                b.push_style(|s| s.fg(CODE));
                b.text(&format!("$${math}$$"));
                b.styles.pop();
            }
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.text("\n"),
            Event::Rule => {
                b.flush_paragraph();
                b.emit(Node::Rule);
            }
            Event::TaskListMarker(done) => b.text(if done { "[x] " } else { "[ ] " }),
        }
    }
    b.flush_paragraph();
    match b.stack.into_iter().next() {
        Some(Frame::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Layout.

/// Pixel sizes of images, when known.
pub type ImageSizes<'a> = &'a dyn Fn(&str) -> Option<(u32, u32)>;

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
}

#[derive(Clone, Copy, PartialEq)]
enum Wrap {
    Word,
    Char,
}

struct Ctx<'a> {
    laid: Laid,
    image_sizes: ImageSizes<'a>,
    /// A terminal cell's size in pixels.
    cell: (u16, u16),
}

/// A character to lay out: its style and its place in the page text, if it has one.
type Cell = (char, Style, Option<usize>);

impl Ctx<'_> {
    fn blank(&mut self) {
        self.laid.rows.push(Row::default());
    }

    /// Adds readable text to the page text; returns where it starts.
    fn record(&mut self, text: &str) -> usize {
        let start = self.laid.text.len();
        self.laid.text.extend(text.chars());
        self.laid.text.push('\n');
        start
    }

    /// Characters of inline runs, recorded in the page text.
    fn cells(&mut self, runs: &[Run]) -> Vec<Cell> {
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        let start = self.record(&text);
        let mut cells = Vec::new();
        let mut i = start;
        for run in runs {
            for c in run.text.chars() {
                cells.push((c, run.style, Some(i)));
                i += 1;
            }
        }
        cells
    }

    /// Lays out cells in rows `width` wide from column `x`.
    fn flow(&mut self, cells: &[Cell], x: u16, width: u16, align: Align, wrap: Wrap) {
        for line in wrap_cells(cells, width, wrap) {
            let line_width: u16 = line.iter().map(|c| char_width(c.0)).sum();
            let x = match align {
                Align::Left => x,
                Align::Center => x + width.saturating_sub(line_width) / 2,
            };
            self.laid.rows.push(Row {
                segs: segs(&line, x),
            });
        }
    }

    fn heading(&mut self, depth: u8, runs: &[Run], x: u16, width: u16, align: Align) {
        let text: String = runs.iter().map(|r| r.text.as_str()).collect();
        // The terminal has one font size, so rank comes from size where it fits
        // (large letters for a short title), then weight and space.
        if depth == 1 && font::width(&text) <= width as usize {
            let w = font::width(&text) as u16;
            let x = if align == Align::Center {
                x + (width - w) / 2
            } else {
                x
            };
            for row in font::render(&text) {
                self.laid.rows.push(Row {
                    segs: vec![Seg {
                        x,
                        text: row,
                        style: Style::default(),
                        offset: None,
                    }],
                });
            }
            return;
        }
        let style = if depth <= 3 {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(DIM)
                .add_modifier(Modifier::BOLD | Modifier::ITALIC)
        };
        let runs: Vec<Run> = runs
            .iter()
            .map(|r| Run {
                text: r.text.clone(),
                style: r.style.patch(style),
            })
            .collect();
        let cells = self.cells(&runs);
        self.flow(&cells, x, width, align, Wrap::Word);
    }

    fn figures(&mut self, hrefs: &[String], x: u16, width: u16) {
        for href in hrefs {
            let (cols, rows) = match (self.image_sizes)(href) {
                Some((w, h)) => image_cells(width, w, h, self.cell),
                None if href.starts_with("http") => (width, REMOTE_IMAGE_ROWS),
                None => continue,
            };
            let top = self.laid.rows.len();
            self.laid.figures.push(Figure {
                href: href.clone(),
                top,
                x: x + (width - cols.min(width)) / 2,
                cols,
                rows,
            });
            for _ in 0..rows {
                self.blank();
            }
        }
    }

    /// A centered block: each line a heading or a line of text.
    fn centered(&mut self, content: &str, x: u16, width: u16) {
        for line in content.lines() {
            if let Some(m) = HEADING.captures(line) {
                let depth = m[1].len() as u8;
                self.blank();
                self.heading(
                    depth,
                    &[Run {
                        text: m[2].to_owned(),
                        style: Style::default(),
                    }],
                    x,
                    width,
                    Align::Center,
                );
                self.blank();
                continue;
            }
            let (text, style) = match line.strip_prefix('*').and_then(|l| l.strip_suffix('*')) {
                Some(inner) if !inner.is_empty() => (
                    inner.replace("\\*", "*"),
                    Style::default().add_modifier(Modifier::ITALIC),
                ),
                _ => (line.replace("\\*", "*"), Style::default()),
            };
            let text = match text.strip_prefix("- ") {
                Some(rest) => format!("• {rest}"),
                None => text,
            };
            let cells = self.cells(&[Run { text, style }]);
            self.flow(&cells, x, width, Align::Center, Wrap::Word);
        }
    }

    fn code(&mut self, code: &str, lang: Option<&str>, x: u16, width: u16) {
        // Code as set, beside a heavy dim rule. Long lines wrap rather than vanish.
        let start = self.record(code);
        let kinds = syntax::kinds(code, lang);
        let mut line_start = 0usize;
        for line in code.split('\n') {
            let cells: Vec<Cell> = line
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    let index = line_start + i;
                    let kind = kinds.get(index).copied().unwrap_or(syntax::Kind::Plain);
                    (c, code_style(kind), Some(start + index))
                })
                .collect();
            line_start += cells.len() + 1;
            let wrapped = if cells.is_empty() {
                vec![Vec::new()]
            } else {
                wrap_cells(&cells, width.saturating_sub(2).max(1), Wrap::Char)
            };
            for part in wrapped {
                let mut row_segs = vec![Seg {
                    x,
                    text: "┃".into(),
                    style: Style::default().fg(DIM),
                    offset: None,
                }];
                row_segs.extend(segs(&part, x + 2));
                self.laid.rows.push(Row { segs: row_segs });
            }
        }
    }

    fn list(&mut self, start: Option<u64>, items: &[Vec<Node>], tight: bool, x: u16, width: u16) {
        let markers: Vec<String> = (0..items.len())
            .map(|i| match start {
                Some(n) => format!("{}.", n + i as u64),
                None => "-".to_owned(),
            })
            .collect();
        let marker_width = markers.iter().map(|m| m.chars().count()).max().unwrap_or(1) as u16;
        for (i, (item, marker)) in items.iter().zip(&markers).enumerate() {
            if i > 0 && !tight {
                self.blank();
            }
            self.hanging(marker, marker_width, item, x, width, !tight);
        }
    }

    /// Blocks hung beside a dim marker `marker_width` wide, as a list item or footnote.
    fn hanging(
        &mut self,
        marker: &str,
        marker_width: u16,
        blocks: &[Node],
        x: u16,
        width: u16,
        spaced: bool,
    ) {
        let first = self.laid.rows.len();
        self.children(
            blocks,
            x + marker_width + 1,
            width.saturating_sub(marker_width + 1).max(1),
            spaced,
        );
        if self.laid.rows.len() == first {
            self.blank();
        }
        let marker = Seg {
            x,
            text: marker.to_owned(),
            style: Style::default().fg(DIM),
            offset: None,
        };
        self.laid.rows[first].segs.insert(0, marker);
    }

    /// A quote is dim and italic beside a dim bar. An alert keeps its text
    /// plain under its kind, beside a bar in the text color.
    fn quote(&mut self, kind: Option<BlockQuoteKind>, blocks: &[Node], x: u16, width: u16) {
        let first = self.laid.rows.len();
        if let Some(kind) = kind {
            let label = match kind {
                BlockQuoteKind::Note => "Note",
                BlockQuoteKind::Tip => "Tip",
                BlockQuoteKind::Important => "Important",
                BlockQuoteKind::Warning => "Warning",
                BlockQuoteKind::Caution => "Caution",
            };
            let style = Style::default().add_modifier(Modifier::BOLD);
            self.laid.rows.push(Row {
                segs: vec![Seg {
                    x: x + 2,
                    text: label.into(),
                    style,
                    offset: None,
                }],
            });
        }
        let body = self.laid.rows.len();
        self.children(blocks, x + 2, width.saturating_sub(2).max(1), true);
        if kind.is_none() {
            for row in &mut self.laid.rows[body..] {
                for seg in &mut row.segs {
                    seg.style = seg.style.fg(DIM).add_modifier(Modifier::ITALIC);
                }
            }
        }
        let bar = if kind.is_some() {
            Style::default()
        } else {
            Style::default().fg(DIM)
        };
        for row in &mut self.laid.rows[first..] {
            row.segs.insert(
                0,
                Seg {
                    x,
                    text: "│".into(),
                    style: bar,
                    offset: None,
                },
            );
        }
    }

    fn table(&mut self, head: &[Inlines], body: &[Vec<Inlines>], x: u16, width: u16) {
        let columns = head.len().max(body.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let rows: Vec<(Vec<Inlines>, bool)> = std::iter::once((head.to_vec(), true))
            .chain(body.iter().map(|r| (r.clone(), false)))
            .collect();
        // Cell characters, recorded in the page text row by row.
        let mut cells: Vec<Vec<Vec<Cell>>> = Vec::new();
        for (row, is_head) in &rows {
            let mut out = Vec::new();
            for c in 0..columns {
                let runs: Vec<Run> = row
                    .get(c)
                    .map(|r| {
                        r.iter()
                            .map(|run| Run {
                                text: run.text.clone(),
                                style: if *is_head {
                                    run.style.patch(bold)
                                } else {
                                    run.style
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(self.cells(&runs));
            }
            cells.push(out);
        }
        let natural = |c: usize| {
            cells
                .iter()
                .map(|row| line_width(&row[c]))
                .max()
                .unwrap_or(0)
                .max(1)
        };
        let mut widths: Vec<u16> = (0..columns).map(natural).collect();
        let available = width.saturating_sub(columns as u16 + 1).max(columns as u16);
        while widths.iter().sum::<u16>() > available {
            let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
                break;
            };
            if widths[widest] <= 1 {
                break;
            }
            widths[widest] -= 1;
        }
        let border = Style::default().fg(DIM);
        let rule = |left: &str, mid: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(*w as usize)).collect();
            Row {
                segs: vec![Seg {
                    x,
                    text: format!("{left}{}{right}", parts.join(mid)),
                    style: border,
                    offset: None,
                }],
            }
        };
        self.laid.rows.push(rule("┌", "┬", "┐"));
        for (r, row) in cells.iter().enumerate() {
            if r > 0 {
                self.laid.rows.push(rule("├", "┼", "┤"));
            }
            let wrapped: Vec<Vec<Vec<Cell>>> = row
                .iter()
                .zip(&widths)
                .map(|(cell, w)| wrap_cells(cell, *w, Wrap::Word))
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            for line in 0..height {
                let mut row_segs = Vec::new();
                let mut cx = x;
                for (c, w) in widths.iter().enumerate() {
                    row_segs.push(Seg {
                        x: cx,
                        text: "│".into(),
                        style: border,
                        offset: None,
                    });
                    if let Some(part) = wrapped[c].get(line) {
                        row_segs.extend(segs(part, cx + 1));
                    }
                    cx += w + 1;
                }
                row_segs.push(Seg {
                    x: cx,
                    text: "│".into(),
                    style: border,
                    offset: None,
                });
                self.laid.rows.push(Row { segs: row_segs });
            }
        }
        self.laid.rows.push(rule("└", "┴", "┘"));
    }

    fn block(&mut self, node: &Node, x: u16, width: u16) {
        match node {
            Node::Paragraph(runs) => {
                let cells = self.cells(runs);
                self.flow(&cells, x, width, Align::Left, Wrap::Word);
            }
            Node::Heading(depth, runs) => self.heading(*depth, runs, x, width, Align::Left),
            Node::Quote(kind, blocks) => self.quote(*kind, blocks, x, width),
            Node::Code { lang, text } => self.code(text, lang.as_deref(), x, width),
            Node::List {
                start,
                items,
                tight,
            } => self.list(*start, items, *tight, x, width),
            Node::Table { head, rows } => self.table(head, rows, x, width),
            Node::Figures(hrefs) => self.figures(hrefs, x, width),
            Node::Centered(content) => {
                let lines: Vec<&str> = content.lines().collect();
                match lines.as_slice() {
                    // A lone heading keeps a heading's spacing.
                    [only] if HEADING.is_match(only) => {
                        let m = HEADING.captures(only).expect("matched");
                        self.heading(
                            m[1].len() as u8,
                            &[Run {
                                text: m[2].to_owned(),
                                style: Style::default(),
                            }],
                            x,
                            width,
                            Align::Center,
                        );
                    }
                    _ => self.centered(content, x, width),
                }
            }
            Node::Footnote(label, blocks) => {
                let marker = format!("[{label}]");
                let marker_width = marker.chars().map(char_width).sum();
                self.hanging(&marker, marker_width, blocks, x, width, true);
            }
            Node::Definitions(blocks) => self.children(blocks, x, width, false),
            Node::Definition(blocks) => {
                self.children(blocks, x + 4, width.saturating_sub(4).max(1), true)
            }
            Node::Rule => {
                self.laid.rows.push(Row {
                    segs: vec![Seg {
                        x,
                        text: "─".repeat(width as usize),
                        style: Style::default().fg(DIM),
                        offset: None,
                    }],
                });
            }
        }
    }

    /// Blocks inside a list item or quote, a blank row apart when `spaced`.
    fn children(&mut self, blocks: &[Node], x: u16, width: u16, spaced: bool) {
        for (i, node) in blocks.iter().enumerate() {
            if i > 0 && spaced {
                self.blank();
            }
            self.block(node, x, width);
        }
    }
}

/// Whether a top-level block has a blank row above as well as below.
fn spaced_above(node: &Node) -> bool {
    match node {
        Node::Heading(..) | Node::Figures(_) => true,
        Node::Centered(content) => {
            let lines: Vec<&str> = content.lines().collect();
            matches!(lines.as_slice(), [only] if HEADING.is_match(only))
        }
        _ => false,
    }
}

fn char_width(c: char) -> u16 {
    if c == '\n' {
        0
    } else {
        c.width().unwrap_or(0) as u16
    }
}

fn line_width(cells: &[Cell]) -> u16 {
    cells
        .split(|c| c.0 == '\n')
        .map(|l| l.iter().map(|c| char_width(c.0)).sum::<u16>())
        .max()
        .unwrap_or(0)
}

/// Breaks cells into lines at most `width` wide: at spaces for `Word`
/// (breaking a word only when it alone is too wide), anywhere for `Char`.
/// Newlines always break. Spaces at a break are dropped.
fn wrap_cells(cells: &[Cell], width: u16, wrap: Wrap) -> Vec<Vec<Cell>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<Cell>> = Vec::new();
    for hard in cells.split(|c| c.0 == '\n') {
        let mut line: Vec<Cell> = Vec::new();
        let mut used: u16 = 0;
        match wrap {
            Wrap::Char => {
                for &cell in hard {
                    let w = char_width(cell.0);
                    if used + w > width && !line.is_empty() {
                        lines.push(std::mem::take(&mut line));
                        used = 0;
                    }
                    line.push(cell);
                    used += w;
                }
            }
            Wrap::Word => {
                // Words with the spaces before them.
                let mut words: Vec<(&[Cell], &[Cell])> = Vec::new();
                let mut i = 0;
                while i < hard.len() {
                    let space_start = i;
                    while i < hard.len() && hard[i].0 == ' ' {
                        i += 1;
                    }
                    let word_start = i;
                    while i < hard.len() && hard[i].0 != ' ' {
                        i += 1;
                    }
                    words.push((&hard[space_start..word_start], &hard[word_start..i]));
                }
                for (space, word) in words {
                    let space_w: u16 = space.iter().map(|c| char_width(c.0)).sum();
                    let word_w: u16 = word.iter().map(|c| char_width(c.0)).sum();
                    if !line.is_empty() && used + space_w + word_w > width {
                        lines.push(std::mem::take(&mut line));
                        used = 0;
                    } else if !line.is_empty() {
                        line.extend_from_slice(space);
                        used += space_w;
                    }
                    if word_w > width {
                        for &cell in word {
                            let w = char_width(cell.0);
                            if used + w > width && !line.is_empty() {
                                lines.push(std::mem::take(&mut line));
                                used = 0;
                            }
                            line.push(cell);
                            used += w;
                        }
                    } else {
                        line.extend_from_slice(word);
                        used += word_w;
                    }
                }
            }
        }
        lines.push(line);
    }
    lines
}

/// Groups a line's cells into same-styled segments starting at column `x`.
fn segs(line: &[Cell], x: u16) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    let mut col = x;
    let mut prev: Option<(Style, Option<usize>)> = None;
    for &(c, style, offset) in line {
        let continues = match (prev, out.last()) {
            (Some((s, Some(o))), Some(_)) => s == style && offset == Some(o + 1),
            (Some((s, None)), Some(_)) => s == style && offset.is_none(),
            _ => false,
        };
        if continues {
            out.last_mut().expect("a segment exists").text.push(c);
        } else {
            out.push(Seg {
                x: col,
                text: c.to_string(),
                style,
                offset,
            });
        }
        prev = Some((style, offset));
        col += char_width(c);
    }
    out
}

/// Lays a page of markdown out `width` columns wide. Each block has a blank
/// row below it; headings and figures have one above too.
pub fn layout(markdown: &str, width: u16, cell: (u16, u16), image_sizes: ImageSizes) -> Laid {
    let mut ctx = Ctx {
        laid: Laid::default(),
        image_sizes,
        cell,
    };
    for node in parse(markdown, true) {
        if spaced_above(&node) {
            ctx.blank();
        }
        ctx.block(&node, 0, width);
        ctx.blank();
    }
    ctx.laid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_images(_: &str) -> Option<(u32, u32)> {
        None
    }

    /// The page as plain rows of text, trailing spaces trimmed.
    fn draw(markdown: &str, width: u16) -> Vec<String> {
        let laid = layout(markdown, width, (8, 16), &no_images);
        laid.rows
            .iter()
            .map(|row| {
                let mut line: Vec<char> = Vec::new();
                for seg in &row.segs {
                    let mut col = seg.x as usize;
                    for c in seg.text.chars() {
                        while line.len() <= col {
                            line.push(' ');
                        }
                        line[col] = c;
                        col += char_width(c).max(1) as usize;
                    }
                }
                line.iter().collect::<String>().trim_end().to_owned()
            })
            .collect()
    }

    #[test]
    fn paragraphs_wrap_at_words_with_a_blank_row_below() {
        assert_eq!(
            draw("one two three four", 9),
            vec!["one two", "three", "four", ""]
        );
    }

    #[test]
    fn headings_have_space_above_and_large_letters_when_short() {
        let rows = draw("# Hi\n\nText", 40);
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], "█ █ █");
        assert_eq!(rows[4], "Text");
        assert_eq!(draw("## Sub", 40), vec!["", "Sub", ""]);
    }

    #[test]
    fn lists_hang_their_text_and_number_from_the_start() {
        assert_eq!(
            draw("3. alpha beta\n4. gamma", 10),
            vec!["3. alpha", "   beta", "4. gamma", ""]
        );
        assert_eq!(draw("- a\n- b", 10), vec!["- a", "- b", ""]);
    }

    #[test]
    fn tables_draw_borders_and_wrap_cells() {
        let rows = draw("| A | B |\n| --- | --- |\n| x | long words here |", 16);
        assert_eq!(rows[0], "┌─┬────────────┐");
        assert_eq!(rows[1], "│A│B           │");
        assert_eq!(rows[3], "│x│long words  │");
        assert_eq!(rows[4], "│ │here        │");
    }

    #[test]
    fn code_keeps_lines_beside_a_rule() {
        assert_eq!(
            draw("```\na = 1\n  b\n```", 20),
            vec!["┃ a = 1", "┃   b", ""]
        );
    }

    fn seg_style(markdown: &str, needle: &str) -> Option<Style> {
        layout(markdown, 80, (8, 16), &no_images)
            .rows
            .iter()
            .flat_map(|row| &row.segs)
            .find(|seg| seg.text.contains(needle))
            .map(|seg| seg.style)
    }

    #[test]
    fn code_is_colored_from_its_fence_or_from_the_source_when_a_pdf_left_it_bare() {
        let keyword = code_style(syntax::Kind::Keyword);
        assert_eq!(seg_style("```rust\nfn main() {}\n```", "fn"), Some(keyword));
        assert_eq!(
            seg_style("```\na = 1\n  b\n```", "a = 1"),
            Some(Style::default())
        );
        assert_eq!(
            seg_style(
                "```\nfor (var i = 0; i < n; i++) {\n    if (x) {\n        go()\n    }\n}\n```",
                "var"
            ),
            Some(keyword)
        );
        let laid = layout("```rust\nfn x\n```", 40, (8, 16), &no_images);
        let text: String = laid.text.iter().collect();
        assert_eq!(text, "fn x\n");
        let seg = laid
            .rows
            .iter()
            .flat_map(|row| &row.segs)
            .find(|seg| seg.text.contains("fn"))
            .unwrap();
        let start = seg.offset.expect("code can be selected");
        assert_eq!(
            laid.text[start..start + seg.text.chars().count()]
                .iter()
                .collect::<String>(),
            seg.text
        );
    }

    #[test]
    fn centered_blocks_center_each_line() {
        let rows = draw("<div align=\"center\">\nab\n*cap*\n</div>", 10);
        assert_eq!(rows, vec!["    ab", "   cap", ""]);
    }

    #[test]
    fn citations_keep_their_brackets_and_links_show_their_url() {
        assert_eq!(
            draw("See [1] and [x](https://e.com).", 60)[0],
            "See [1] and x (https://e.com)."
        );
    }

    #[test]
    fn page_text_offsets_point_at_the_drawn_characters() {
        let laid = layout("Hello world\n\nNext", 6, (8, 16), &no_images);
        let text: String = laid.text.iter().collect();
        assert_eq!(text, "Hello world\nNext\n");
        let world = &laid.rows[1].segs[0];
        assert_eq!((world.text.as_str(), world.offset), ("world", Some(6)));
    }

    #[test]
    fn footnotes_keep_their_label_in_text_and_hang_beside_it() {
        assert_eq!(
            draw("Fact[^1].\n\n[^1]: Source here.", 40),
            vec!["Fact[1].", "", "[1] Source here.", ""]
        );
    }

    #[test]
    fn alerts_show_their_kind_and_quotes_a_bar() {
        assert_eq!(
            draw("> [!WARNING]\n> Hot.", 20),
            vec!["│ Warning", "│ Hot.", ""]
        );
        assert_eq!(draw("> said", 20), vec!["│ said", ""]);
    }

    #[test]
    fn definitions_sit_under_their_term() {
        assert_eq!(draw("Term\n: Meaning", 20), vec!["Term", "    Meaning", ""]);
    }

    #[test]
    fn math_keeps_its_source_and_br_breaks_the_line() {
        assert_eq!(
            draw("Area $a^2$ here<br>next", 40),
            vec!["Area $a^2$ here", "next", ""]
        );
    }

    #[test]
    fn html_blocks_are_read_as_markdown() {
        assert_eq!(
            draw(
                "<details>\n<summary>More</summary>\n<p>Hidden <b>text</b></p>\n</details>",
                40
            )[..2],
            ["More", ""]
        );
        let sizes = |_: &str| Some((160, 80));
        let laid = layout(
            "<p align=\"center\"><img src=\"leaf-image:0\"></p>",
            40,
            (8, 16),
            &sizes,
        );
        assert_eq!(laid.figures.len(), 1);
    }

    #[test]
    fn images_alone_in_a_paragraph_hold_space_as_figures() {
        let sizes = |_: &str| Some((160, 80));
        let laid = layout("![](leaf-image:a)", 76, (8, 16), &sizes);
        assert_eq!(
            laid.figures,
            vec![Figure {
                href: "leaf-image:a".into(),
                top: 1,
                x: 28,
                cols: 20,
                rows: 5
            }]
        );
        assert_eq!(laid.rows.len(), 1 + 5 + 1);
    }
}
