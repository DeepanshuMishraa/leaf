//! A Word document, read by leaf itself. Shown as designed, it's laid out
//! on the document's own page size and margins, in its own fonts, sizes,
//! colors, indents, lists, tables and pictures, and drawn page by page.
//! Shown in the terminal's font, each page's words read as markdown:
//! headings, emphasis, lists, tables and pictures. Both looks share the
//! same pages, so switching keeps the reader's place.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;

use image::RgbaImage;

use crate::doc::{Doc, Look, Source};
use crate::drawing::{self, Canvas, Rect};
use crate::ooxml::{
    Color, Local, Package, PackageFailed, Rel, Theme, data_uri, emu, md_escape, rel_by_id,
    rel_by_kind, roxmltree,
};
use crate::typeset::{self, Line, Run, Style};
use roxmltree::{Document, Node};

/// Width pages are measured at before the screen picks the real one.
const MEASURE_WIDTH: u32 = 1600;
/// Widest a page is drawn, in pixels.
const MAX_WIDTH: u32 = 3200;
/// A tab, as the room it takes.
const TAB: &str = "    ";
/// Room between a cell's border and its text, left and right, in points.
const CELL_PAD_X: f32 = 5.4;
/// The same, above and below.
const CELL_PAD_Y: f32 = 2.0;

fn twips(value: &str) -> Option<f32> {
    value.parse::<f32>().ok().map(|v| v / 20.0)
}

/// Whether an on/off property such as `<w:b/>` is on.
fn on(node: Node) -> bool {
    !matches!(node.attr("val"), Some("0" | "false" | "off" | "none"))
}

// ---------------------------------------------------------------------------
// Formatting, as styles and direct formatting set it. Unset fields inherit.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Spacing {
    /// A multiple of the font's line height.
    Auto(f32),
    /// Exactly this many points.
    Exact(f32),
    /// At least this many points.
    AtLeast(f32),
}

#[derive(Debug, Clone, Default, PartialEq)]
struct RunFmt {
    font: Option<String>,
    size: Option<f32>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    caps: Option<bool>,
    hidden: Option<bool>,
    color: Option<Color>,
    baseline: Option<f32>,
}

impl RunFmt {
    fn apply(&mut self, rpr: Node, theme: &Theme) {
        for n in rpr.children().filter(|n| n.is_element()) {
            match n.local() {
                "rFonts" => {
                    let named = n.attr("ascii").or_else(|| n.attr("hAnsi"));
                    let themed = n.attr("asciiTheme").or_else(|| n.attr("hAnsiTheme"));
                    if let Some(theme_font) = themed {
                        self.font = Some(if theme_font.starts_with("major") {
                            theme.major.clone()
                        } else {
                            theme.minor.clone()
                        });
                    } else if let Some(name) = named {
                        self.font = Some(name.to_owned());
                    }
                }
                "sz" => {
                    if let Some(size) = n.attr("val").and_then(|v| v.parse::<f32>().ok()) {
                        self.size = Some(size / 2.0);
                    }
                }
                "b" => self.bold = Some(on(n)),
                "i" => self.italic = Some(on(n)),
                "u" => self.underline = Some(on(n)),
                "strike" | "dstrike" => self.strike = Some(on(n)),
                "caps" | "smallCaps" => self.caps = Some(on(n)),
                "vanish" => self.hidden = Some(on(n)),
                "color" => {
                    self.color = match n.attr("val") {
                        Some("auto") => Some(Color::BLACK),
                        Some(hex) => Color::hex(hex),
                        None => None,
                    }
                    .or_else(|| n.attr("themeColor").and_then(|t| theme_color(theme, t)));
                }
                "vertAlign" => {
                    self.baseline = match n.attr("val") {
                        Some("superscript") => Some(0.33),
                        Some("subscript") => Some(-0.15),
                        _ => Some(0.0),
                    };
                }
                _ => {}
            }
        }
    }

    fn merge(&mut self, over: &RunFmt) {
        macro_rules! take {
            ($($f:ident),*) => { $( if over.$f.is_some() { self.$f = over.$f.clone(); } )* };
        }
        take!(
            font, size, bold, italic, underline, strike, caps, hidden, color, baseline
        );
    }

    fn style(&self) -> Style {
        Style {
            family: self.font.clone().unwrap_or_else(|| "Calibri".into()),
            size: self.size.unwrap_or(10.0),
            bold: self.bold.unwrap_or(false),
            italic: self.italic.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            strike: self.strike.unwrap_or(false),
            color: self.color.unwrap_or(Color::BLACK),
            baseline: self.baseline.unwrap_or(0.0),
            caps: self.caps.unwrap_or(false),
        }
    }
}

/// A Word theme color name, such as `accent1` or `text1`, as a color.
fn theme_color(theme: &Theme, name: &str) -> Option<Color> {
    let slot = match name {
        "dark1" | "text1" => "dk1",
        "light1" | "background1" => "lt1",
        "dark2" | "text2" => "dk2",
        "light2" | "background2" => "lt2",
        "hyperlink" => "hlink",
        "followedHyperlink" => "folHlink",
        other => other,
    };
    theme.scheme(slot)
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ParaFmt {
    align: Option<Align>,
    before: Option<f32>,
    after: Option<f32>,
    line: Option<Spacing>,
    left: Option<f32>,
    right: Option<f32>,
    /// The first line's indent from `left`; negative for a hanging indent.
    first: Option<f32>,
    outline: Option<u8>,
    /// The list (`numId`) and level the paragraph is numbered with.
    num: Option<(String, u8)>,
    break_before: Option<bool>,
    shade: Option<Color>,
}

impl ParaFmt {
    fn apply(&mut self, ppr: Node) {
        for n in ppr.children().filter(|n| n.is_element()) {
            match n.local() {
                "jc" => {
                    self.align = match n.attr("val") {
                        Some("center") => Some(Align::Center),
                        Some("right" | "end") => Some(Align::Right),
                        _ => Some(Align::Left),
                    };
                }
                "spacing" => {
                    if let Some(v) = n.attr("before").and_then(twips) {
                        self.before = Some(v);
                    }
                    if let Some(v) = n.attr("after").and_then(twips) {
                        self.after = Some(v);
                    }
                    if n.attr("beforeAutospacing").is_some_and(|v| v != "0") {
                        self.before = Some(14.0);
                    }
                    if n.attr("afterAutospacing").is_some_and(|v| v != "0") {
                        self.after = Some(14.0);
                    }
                    if let Some(line) = n.attr("line").and_then(|v| v.parse::<f32>().ok()) {
                        self.line = Some(match n.attr("lineRule") {
                            Some("exact") => Spacing::Exact(line / 20.0),
                            Some("atLeast") => Spacing::AtLeast(line / 20.0),
                            _ => Spacing::Auto(line / 240.0),
                        });
                    }
                }
                "ind" => {
                    if let Some(v) = n.attr("left").or_else(|| n.attr("start")).and_then(twips) {
                        self.left = Some(v);
                    }
                    if let Some(v) = n.attr("right").or_else(|| n.attr("end")).and_then(twips) {
                        self.right = Some(v);
                    }
                    if let Some(v) = n.attr("firstLine").and_then(twips) {
                        self.first = Some(v);
                    }
                    if let Some(v) = n.attr("hanging").and_then(twips) {
                        self.first = Some(-v);
                    }
                }
                "outlineLvl" => {
                    self.outline = n.attr("val").and_then(|v| v.parse().ok());
                }
                "numPr" => {
                    let id = n.child("numId").and_then(|i| i.attr("val"));
                    let level = n
                        .child("ilvl")
                        .and_then(|i| i.attr("val"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    if let Some(id) = id {
                        self.num = Some((id.to_owned(), level));
                    }
                }
                "pageBreakBefore" => self.break_before = Some(on(n)),
                "shd" => {
                    self.shade = n.attr("fill").filter(|f| *f != "auto").and_then(Color::hex);
                }
                _ => {}
            }
        }
    }

    fn merge(&mut self, over: &ParaFmt) {
        macro_rules! take {
            ($($f:ident),*) => { $( if over.$f.is_some() { self.$f = over.$f.clone(); } )* };
        }
        take!(
            align,
            before,
            after,
            line,
            left,
            right,
            first,
            outline,
            num,
            break_before,
            shade
        );
    }
}

// ---------------------------------------------------------------------------
// Styles and numbering.

#[derive(Debug, Default)]
struct StyleDef {
    name: String,
    based_on: Option<String>,
    para: ParaFmt,
    run: RunFmt,
    /// For a table style: whether it draws cell borders.
    borders: bool,
}

#[derive(Debug, Default)]
struct Styles {
    para: ParaFmt,
    run: RunFmt,
    defs: HashMap<String, StyleDef>,
    default_para: Option<String>,
    default_table: Option<String>,
}

/// Whether a `tblBorders` element draws any border.
fn has_borders(borders: Node) -> bool {
    borders
        .children()
        .filter(|n| n.is_element())
        .any(|b| !matches!(b.attr("val"), Some("nil" | "none")))
}

impl Styles {
    fn parse(xml: Option<&str>, theme: &Theme) -> Styles {
        let mut styles = Styles::default();
        let Some(doc) = xml.and_then(|x| Document::parse(x).ok()) else {
            return styles;
        };
        let root = doc.root_element();
        if let Some(defaults) = root.child("docDefaults") {
            if let Some(rpr) = defaults.path(&["rPrDefault", "rPr"]) {
                styles.run.apply(rpr, theme);
            }
            if let Some(ppr) = defaults.path(&["pPrDefault", "pPr"]) {
                styles.para.apply(ppr);
            }
        }
        for s in root.kids("style") {
            let Some(id) = s.attr("styleId") else {
                continue;
            };
            let kind = s.attr("type").unwrap_or("paragraph");
            if s.attr("default").is_some_and(|d| d == "1" || d == "true") {
                match kind {
                    "paragraph" => styles.default_para = Some(id.to_owned()),
                    "table" => styles.default_table = Some(id.to_owned()),
                    _ => {}
                }
            }
            let mut def = StyleDef {
                name: s
                    .child("name")
                    .and_then(|n| n.attr("val"))
                    .unwrap_or(id)
                    .to_ascii_lowercase(),
                based_on: s
                    .child("basedOn")
                    .and_then(|b| b.attr("val"))
                    .map(str::to_owned),
                ..StyleDef::default()
            };
            if let Some(ppr) = s.child("pPr") {
                def.para.apply(ppr);
            }
            if let Some(rpr) = s.child("rPr") {
                def.run.apply(rpr, theme);
            }
            def.borders = s.path(&["tblPr", "tblBorders"]).is_some_and(has_borders);
            styles.defs.insert(id.to_owned(), def);
        }
        styles
    }

    /// A style and those it's based on, the base first.
    fn chain(&self, id: Option<&str>) -> Vec<&StyleDef> {
        let mut out = Vec::new();
        let mut next = id;
        while let Some(id) = next {
            let Some(def) = self.defs.get(id) else { break };
            out.push(def);
            if out.len() > 16 {
                break;
            }
            next = def.based_on.as_deref();
        }
        out.reverse();
        out
    }

    /// A paragraph style's formatting, over the document's defaults.
    fn paragraph(&self, id: Option<&str>) -> (ParaFmt, RunFmt) {
        let (mut para, mut run) = (self.para.clone(), self.run.clone());
        for def in self.chain(id.or(self.default_para.as_deref())) {
            para.merge(&def.para);
            run.merge(&def.run);
        }
        (para, run)
    }

    /// A character style's run formatting.
    fn character(&self, id: &str) -> RunFmt {
        let mut run = RunFmt::default();
        for def in self.chain(Some(id)) {
            run.merge(&def.run);
        }
        run
    }

    /// The heading level a style's name gives: 0 for a title, 1 for Heading 1.
    fn heading(&self, id: Option<&str>) -> Option<u8> {
        let name = &self.defs.get(id?)?.name;
        if name == "title" {
            return Some(0);
        }
        name.strip_prefix("heading ")?.trim().parse().ok()
    }

    fn table_borders(&self, id: Option<&str>) -> bool {
        self.chain(id.or(self.default_table.as_deref()))
            .iter()
            .any(|d| d.borders)
    }
}

#[derive(Debug, Clone, Default)]
struct Level {
    format: String,
    text: String,
    start: u32,
    para: ParaFmt,
}

#[derive(Debug, Default)]
struct Numbering {
    /// Each list's abstract definition and its levels.
    lists: HashMap<String, (String, Vec<Level>)>,
    counters: HashMap<String, [u32; 9]>,
}

impl Numbering {
    fn parse(xml: Option<&str>) -> Numbering {
        let mut numbering = Numbering::default();
        let Some(doc) = xml.and_then(|x| Document::parse(x).ok()) else {
            return numbering;
        };
        let root = doc.root_element();
        let mut abstracts: HashMap<&str, Vec<Level>> = HashMap::new();
        for a in root.kids("abstractNum") {
            let Some(id) = a.attr("abstractNumId") else {
                continue;
            };
            let mut levels = vec![Level::default(); 9];
            for lvl in a.kids("lvl") {
                let Some(i) = lvl.attr("ilvl").and_then(|v| v.parse::<usize>().ok()) else {
                    continue;
                };
                let Some(level) = levels.get_mut(i) else {
                    continue;
                };
                level.format = lvl
                    .child("numFmt")
                    .and_then(|n| n.attr("val"))
                    .unwrap_or("decimal")
                    .to_owned();
                level.text = lvl
                    .child("lvlText")
                    .and_then(|n| n.attr("val"))
                    .unwrap_or("")
                    .to_owned();
                level.start = lvl
                    .child("start")
                    .and_then(|n| n.attr("val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1);
                if let Some(ppr) = lvl.child("pPr") {
                    level.para.apply(ppr);
                }
            }
            abstracts.insert(id, levels);
        }
        for num in root.kids("num") {
            let (Some(id), Some(abs)) = (
                num.attr("numId"),
                num.child("abstractNumId").and_then(|a| a.attr("val")),
            ) else {
                continue;
            };
            if let Some(levels) = abstracts.get(abs) {
                numbering
                    .lists
                    .insert(id.to_owned(), (abs.to_owned(), levels.clone()));
            }
        }
        numbering
    }

    fn level(&self, id: &str, level: u8) -> Option<&Level> {
        self.lists.get(id)?.1.get(level as usize)
    }

    /// The next label for a list item, counting it.
    fn label(&mut self, id: &str, level: u8) -> Option<Mark> {
        let (key, levels) = self.lists.get(id)?;
        let at = level as usize;
        let this = levels.get(at)?;
        let counters = self.counters.entry(key.clone()).or_insert([0; 9]);
        for (i, counter) in counters.iter_mut().enumerate() {
            if i < at && *counter == 0 {
                *counter = levels[i].start;
            }
        }
        counters[at] = if counters[at] == 0 {
            this.start
        } else {
            counters[at] + 1
        };
        for counter in counters.iter_mut().skip(at + 1) {
            *counter = 0;
        }
        if this.format == "none" || this.text.is_empty() {
            return None;
        }
        if this.format == "bullet" {
            return Some(Mark::Bullet(bullet(&this.text)));
        }
        let mut text = this.text.clone();
        for (i, level) in levels.iter().enumerate().take(9) {
            let n = counters[i].max(1);
            text = text.replace(&format!("%{}", i + 1), &number(&level.format, n));
        }
        Some(Mark::Number(text))
    }
}

/// A Word number format's digits for `n`.
fn number(format: &str, n: u32) -> String {
    let scheme = match format {
        "lowerLetter" => "alphaLcPlain",
        "upperLetter" => "alphaUcPlain",
        "lowerRoman" => "romanLcPlain",
        "upperRoman" => "romanUcPlain",
        _ => "arabicPlain",
    };
    drawing::number_label(scheme, n)
}

/// A bullet as a character every font has: Word's often come from Symbol
/// or Wingdings, whose private-use characters draw as boxes elsewhere.
fn bullet(text: &str) -> String {
    match text.chars().next() {
        Some('o') => "◦".into(),
        Some('\u{f0a7}' | '§' | '\u{f06e}') => "▪".into(),
        Some('\u{f0d8}') => "➢".into(),
        Some('\u{f0fc}') => "✓".into(),
        Some('-' | '–' | '•' | '◦' | '▪' | '■' | '►' | '➢' | '✓' | '*') => {
            text.into()
        }
        _ => "•".into(),
    }
}

// ---------------------------------------------------------------------------
// The document, as blocks.

#[derive(Debug, Clone, PartialEq)]
enum Mark {
    Bullet(String),
    Number(String),
}

impl Mark {
    fn text(&self) -> &str {
        match self {
            Mark::Bullet(t) | Mark::Number(t) => t,
        }
    }
}

#[derive(Debug, Clone)]
enum Inline {
    Text(String, Rc<Style>),
    Picture { part: String, w: f32, h: f32 },
    PageBreak,
}

#[derive(Debug, Clone)]
struct Para {
    fmt: ParaFmt,
    /// The paragraph mark's style: an empty paragraph is this tall.
    mark: Rc<Style>,
    heading: Option<u8>,
    list: Option<(Mark, u8)>,
    items: Vec<Inline>,
}

#[derive(Debug, Clone)]
struct TableCell {
    paras: Vec<Para>,
    span: usize,
    shade: Option<Color>,
    /// Continues the merged cell above it.
    continued: bool,
}

#[derive(Debug, Clone)]
struct Table {
    grid: Vec<f32>,
    rows: Vec<Vec<TableCell>>,
    borders: bool,
}

#[derive(Debug, Clone)]
enum Block {
    Para(Para),
    Table(Table),
    PageBreak,
}

struct Reader<'a> {
    styles: &'a Styles,
    numbering: Numbering,
    theme: &'a Theme,
    rels: &'a [Rel],
    /// For each open field, whether its code (not its result) is being read.
    fields: Vec<bool>,
}

impl Reader<'_> {
    fn blocks(&mut self, parent: Node, out: &mut Vec<Block>) {
        for child in parent.children().filter(|n| n.is_element()) {
            match child.local() {
                "p" => self.paragraph(child, out),
                "tbl" => out.push(Block::Table(self.table(child))),
                "sdt" => {
                    if let Some(content) = child.child("sdtContent") {
                        self.blocks(content, out);
                    }
                }
                "customXml" | "ins" | "moveTo" => self.blocks(child, out),
                _ => {}
            }
        }
    }

    fn paragraph(&mut self, p: Node, out: &mut Vec<Block>) {
        let ppr = p.child("pPr");
        let style_id = ppr
            .and_then(|n| n.child("pStyle"))
            .and_then(|n| n.attr("val"));
        let (mut fmt, base_run) = self.styles.paragraph(style_id);
        let mut direct = ParaFmt::default();
        if let Some(ppr) = ppr {
            direct.apply(ppr);
        }
        let num = direct.num.clone().or_else(|| fmt.num.clone());
        let mut list = None;
        if let Some((id, level)) = num.filter(|(id, _)| id != "0") {
            if let Some(lvl) = self.numbering.level(&id, level) {
                let para = lvl.para.clone();
                fmt.merge(&para);
            }
            list = self.numbering.label(&id, level).map(|m| (m, level));
        }
        fmt.merge(&direct);
        let mut mark = base_run.clone();
        if let Some(rpr) = ppr.and_then(|n| n.child("rPr")) {
            mark.apply(rpr, self.theme);
        }
        let heading = fmt.outline.filter(|l| *l < 9).map(|l| l + 1).or_else(|| {
            self.styles
                .heading(style_id.or(self.styles.default_para.as_deref()))
        });
        let mut items = Vec::new();
        self.inlines(p, &base_run, &mut items);
        if fmt.break_before == Some(true) {
            out.push(Block::PageBreak);
        }
        out.push(Block::Para(Para {
            fmt,
            mark: Rc::new(mark.style()),
            heading,
            list,
            items,
        }));
        // A section break inside a paragraph starts a new page, unless continuous.
        if let Some(sect) = ppr.and_then(|n| n.child("sectPr")) {
            let continuous = sect
                .child("type")
                .and_then(|t| t.attr("val"))
                .is_some_and(|v| v == "continuous");
            if !continuous {
                out.push(Block::PageBreak);
            }
        }
    }

    fn inlines(&mut self, node: Node, base: &RunFmt, out: &mut Vec<Inline>) {
        for child in node.children().filter(|n| n.is_element()) {
            match child.local() {
                "r" => self.run(child, base, out),
                "hyperlink" | "ins" | "moveTo" | "smartTag" | "fldSimple" | "customXml" | "bdo"
                | "dir" => self.inlines(child, base, out),
                "sdt" => {
                    if let Some(content) = child.child("sdtContent") {
                        self.inlines(content, base, out);
                    }
                }
                _ => {}
            }
        }
    }

    fn run(&mut self, r: Node, base: &RunFmt, out: &mut Vec<Inline>) {
        let mut fmt = base.clone();
        let rpr = r.child("rPr");
        if let Some(id) = rpr
            .and_then(|n| n.child("rStyle"))
            .and_then(|n| n.attr("val"))
        {
            fmt.merge(&self.styles.character(id));
        }
        if let Some(rpr) = rpr {
            let mut direct = RunFmt::default();
            direct.apply(rpr, self.theme);
            fmt.merge(&direct);
        }
        let hidden = fmt.hidden == Some(true);
        let style = Rc::new(fmt.style());
        let push = |out: &mut Vec<Inline>, text: &str| {
            if let Some(Inline::Text(last, last_style)) = out.last_mut()
                && Rc::ptr_eq(last_style, &style)
            {
                last.push_str(text);
                return;
            }
            out.push(Inline::Text(text.to_owned(), style.clone()));
        };
        for c in r.children().filter(|n| n.is_element()) {
            let in_code = self.fields.last().copied().unwrap_or(false);
            match c.local() {
                "fldChar" => match c.attr("fldCharType") {
                    Some("begin") => self.fields.push(true),
                    Some("separate") => {
                        if let Some(code) = self.fields.last_mut() {
                            *code = false;
                        }
                    }
                    Some("end") => {
                        self.fields.pop();
                    }
                    _ => {}
                },
                _ if in_code || hidden => {}
                "t" => push(out, c.text().unwrap_or("")),
                "tab" | "ptab" => push(out, TAB),
                "br" => match c.attr("type") {
                    Some("page") => out.push(Inline::PageBreak),
                    Some("column") => {}
                    _ => push(out, "\n"),
                },
                "cr" => push(out, "\n"),
                "noBreakHyphen" => push(out, "-"),
                "drawing" => {
                    if let Some(picture) = self.drawing(c) {
                        out.push(picture);
                    }
                }
                "pict" | "object" => {
                    if let Some(picture) = self.vml(c) {
                        out.push(picture);
                    }
                }
                _ => {}
            }
        }
    }

    fn drawing(&self, drawing: Node) -> Option<Inline> {
        let frame = drawing
            .children()
            .find(|n| n.has_tag_name_local("inline") || n.has_tag_name_local("anchor"))?;
        let extent = frame.child("extent")?;
        let w = extent.attr("cx").and_then(emu)?;
        let h = extent.attr("cy").and_then(emu)?;
        let id = frame
            .descendants()
            .find(|n| n.has_tag_name_local("blip"))?
            .attr("embed")?;
        let rel = rel_by_id(self.rels, id).filter(|r| !r.external)?;
        Some(Inline::Picture {
            part: rel.target.clone(),
            w,
            h,
        })
    }

    /// A picture in older VML markup, sized by its shape's CSS.
    fn vml(&self, pict: Node) -> Option<Inline> {
        let data = pict
            .descendants()
            .find(|n| n.has_tag_name_local("imagedata"))?;
        let rel = rel_by_id(self.rels, data.attr("id")?).filter(|r| !r.external)?;
        let css = data.parent()?.attr("style").unwrap_or("");
        let length = |key: &str| {
            css.split(';').find_map(|decl| {
                let (k, v) = decl.split_once(':')?;
                if k.trim() != key {
                    return None;
                }
                let v = v.trim();
                v.strip_suffix("pt")
                    .and_then(|n| n.parse().ok())
                    .or_else(|| {
                        v.strip_suffix("in")
                            .and_then(|n| n.parse::<f32>().ok())
                            .map(|n| n * 72.0)
                    })
            })
        };
        Some(Inline::Picture {
            part: rel.target.clone(),
            w: length("width").unwrap_or(200.0),
            h: length("height").unwrap_or(150.0),
        })
    }

    fn table(&mut self, tbl: Node) -> Table {
        let grid: Vec<f32> = tbl
            .child("tblGrid")
            .map(|g| {
                g.kids("gridCol")
                    .map(|c| c.attr("w").and_then(twips).unwrap_or(0.0))
                    .collect()
            })
            .unwrap_or_default();
        let props = tbl.child("tblPr");
        let style = props
            .and_then(|p| p.child("tblStyle"))
            .and_then(|s| s.attr("val"));
        let borders = match props.and_then(|p| p.child("tblBorders")) {
            Some(b) => has_borders(b),
            None => self.styles.table_borders(style),
        };
        let mut rows = Vec::new();
        for tr in tbl.kids("tr") {
            let mut cells = Vec::new();
            for tc in tr.kids("tc") {
                let tcpr = tc.child("tcPr");
                let span = tcpr
                    .and_then(|p| p.child("gridSpan"))
                    .and_then(|s| s.attr("val"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(1usize)
                    .max(1);
                let continued = tcpr
                    .and_then(|p| p.child("vMerge"))
                    .is_some_and(|m| m.attr("val") != Some("restart"));
                let shade = tcpr
                    .and_then(|p| p.child("shd"))
                    .and_then(|s| s.attr("fill"))
                    .filter(|f| *f != "auto")
                    .and_then(Color::hex);
                let mut blocks = Vec::new();
                self.blocks(tc, &mut blocks);
                cells.push(TableCell {
                    paras: flatten(blocks),
                    span,
                    shade,
                    continued,
                });
            }
            rows.push(cells);
        }
        Table {
            grid,
            rows,
            borders,
        }
    }
}

/// A cell's blocks as paragraphs: a table nested in it gives its cells'.
fn flatten(blocks: Vec<Block>) -> Vec<Para> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            Block::Para(p) => out.push(p),
            Block::Table(t) => {
                for cell in t.rows.into_iter().flatten() {
                    out.extend(cell.paras);
                }
            }
            Block::PageBreak => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Laying the document out on pages.

#[derive(Debug, Clone, Copy)]
struct Geometry {
    width: f32,
    height: f32,
    top: f32,
    bottom: f32,
    left: f32,
    right: f32,
}

impl Geometry {
    fn parse(sect: Option<Node>) -> Geometry {
        let mut g = Geometry {
            width: 612.0,
            height: 792.0,
            top: 72.0,
            bottom: 72.0,
            left: 72.0,
            right: 72.0,
        };
        let Some(sect) = sect else { return g };
        if let Some(size) = sect.child("pgSz") {
            g.width = size.attr("w").and_then(twips).unwrap_or(g.width);
            g.height = size.attr("h").and_then(twips).unwrap_or(g.height);
        }
        if let Some(m) = sect.child("pgMar") {
            let get = |k: &str, d: f32| m.attr(k).and_then(twips).map_or(d, f32::abs);
            g.top = get("top", g.top);
            g.bottom = get("bottom", g.bottom);
            g.left = get("left", g.left);
            g.right = get("right", g.right);
        }
        // A page too small to hold text would lay out forever.
        g.width = g.width.max(g.left + g.right + 72.0);
        g.height = g.height.max(g.top + g.bottom + 72.0);
        g
    }

    fn text_width(&self) -> f32 {
        self.width - self.left - self.right
    }
}

#[derive(Debug, Clone)]
enum Item {
    Text { line: Line, x: f32, baseline: f32 },
    Picture { part: String, rect: Rect },
    Fill { rect: Rect, color: Color },
    Rule { x1: f32, y1: f32, x2: f32, y2: f32 },
}

impl Item {
    fn moved(mut self, dx: f32, dy: f32) -> Item {
        match &mut self {
            Item::Text { x, baseline, .. } => {
                *x += dx;
                *baseline += dy;
            }
            Item::Picture { rect, .. } | Item::Fill { rect, .. } => {
                rect.x += dx;
                rect.y += dy;
            }
            Item::Rule { x1, y1, x2, y2 } => {
                *x1 += dx;
                *x2 += dx;
                *y1 += dy;
                *y2 += dy;
            }
        }
        self
    }
}

/// One line of a paragraph (or a picture), with what's drawn on it relative
/// to its top left.
#[derive(Debug, Clone, Default)]
struct Row {
    height: f32,
    items: Vec<Item>,
    /// Starts a new page.
    page_break: bool,
}

fn line_height(natural: f32, spacing: Option<Spacing>) -> f32 {
    match spacing {
        Some(Spacing::Auto(m)) => natural * m,
        Some(Spacing::Exact(v)) => v,
        Some(Spacing::AtLeast(v)) => natural.max(v),
        None => natural,
    }
}

/// A paragraph's rows in a column `width` points wide, with the room it
/// keeps above and below.
fn lay_para(para: &Para, width: f32) -> (f32, Vec<Row>, f32) {
    let fmt = &para.fmt;
    let left = fmt.left.unwrap_or(0.0);
    let right = fmt.right.unwrap_or(0.0);
    let first = fmt.first.unwrap_or(0.0);
    let align = fmt.align.unwrap_or_default();
    let mut rows = Vec::new();
    let first_style = para
        .items
        .iter()
        .find_map(|i| match i {
            Inline::Text(_, s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| para.mark.clone());
    // A list label hangs at the first line's indent; the text starts after it.
    let label = para.list.as_ref().map(|(mark, _)| {
        let style = Rc::new(Style {
            underline: false,
            strike: false,
            ..(*first_style).clone()
        });
        let text = mark.text().to_owned();
        let w = typeset::measure(&text, &style);
        (text, style, w)
    });
    let label_x = left + first;
    let text_x = match &label {
        Some((_, _, w)) => {
            let after = label_x + w + first_style.size * 0.5;
            if first < 0.0 && after <= left {
                left
            } else {
                after
            }
        }
        None => label_x,
    };
    let mut segments: Vec<Vec<Run>> = vec![Vec::new()];
    let row_for = |line: Line, x: f32, room: f32, label: Option<&(String, Rc<Style>, f32)>| {
        let natural = line.height();
        let height = line_height(natural, fmt.line);
        let baseline = (height - natural) + line.ascent;
        let shift = match align {
            Align::Left => 0.0,
            Align::Center => ((room - line.width) / 2.0).max(0.0),
            Align::Right => (room - line.width).max(0.0),
        };
        let mut items = Vec::new();
        if let Some((text, style, w)) = label {
            let mark = Line {
                pieces: vec![typeset::Piece {
                    x: 0.0,
                    width: *w,
                    text: text.clone(),
                    style: style.clone(),
                }],
                width: *w,
                ascent: line.ascent,
                descent: line.descent,
                leading: line.leading,
            };
            items.push(Item::Text {
                line: mark,
                x: label_x + shift,
                baseline,
            });
        }
        items.push(Item::Text {
            line,
            x: x + shift,
            baseline,
        });
        Row {
            height,
            items,
            page_break: false,
        }
    };
    let mut first_line = true;
    let mut break_next = false;
    let flush =
        |runs: &mut Vec<Run>, rows: &mut Vec<Row>, first_line: &mut bool, break_next: &mut bool| {
            if runs.is_empty() {
                return;
            }
            let first_room = width - right - if *first_line { text_x } else { left };
            let rest_room = width - right - left;
            for (i, line) in typeset::wrap(runs, first_room, rest_room, &para.mark)
                .into_iter()
                .enumerate()
            {
                let lead = *first_line && i == 0;
                let x = if lead { text_x } else { left };
                let room = if lead { first_room } else { rest_room };
                let mut row = row_for(line, x, room, if lead { label.as_ref() } else { None });
                row.page_break = std::mem::take(break_next);
                rows.push(row);
            }
            *first_line = false;
            runs.clear();
        };
    for item in &para.items {
        match item {
            Inline::Text(text, style) => {
                if let Some(runs) = segments.last_mut() {
                    runs.push(Run {
                        text: text.clone(),
                        style: style.clone(),
                    });
                }
            }
            Inline::Picture { part, w, h } => {
                if let Some(runs) = segments.last_mut() {
                    flush(runs, &mut rows, &mut first_line, &mut break_next);
                }
                let room = (width - left - right).max(1.0);
                let scale = (room / w.max(1.0)).min(1.0);
                let (w, h) = (w * scale, h * scale);
                let x = left
                    + match align {
                        Align::Left => 0.0,
                        Align::Center => (room - w) / 2.0,
                        Align::Right => room - w,
                    };
                rows.push(Row {
                    height: h,
                    items: vec![Item::Picture {
                        part: part.clone(),
                        rect: Rect { x, y: 0.0, w, h },
                    }],
                    page_break: std::mem::take(&mut break_next),
                });
                first_line = false;
            }
            Inline::PageBreak => {
                if let Some(runs) = segments.last_mut() {
                    flush(runs, &mut rows, &mut first_line, &mut break_next);
                }
                break_next = true;
            }
        }
    }
    if let Some(runs) = segments.last_mut() {
        flush(runs, &mut rows, &mut first_line, &mut break_next);
    }
    if rows.is_empty() {
        let line = typeset::wrap(&[], width, width, &para.mark)
            .into_iter()
            .next()
            .unwrap_or_default();
        let mut row = row_for(line, text_x, width - right - text_x, label.as_ref());
        row.page_break = break_next;
        rows.push(row);
    }
    if let Some(color) = fmt.shade {
        let total: f32 = rows.iter().map(|r| r.height).sum();
        if let Some(row) = rows.first_mut() {
            row.items.insert(
                0,
                Item::Fill {
                    rect: Rect {
                        x: left,
                        y: 0.0,
                        w: width - left - right,
                        h: total,
                    },
                    color,
                },
            );
        }
    }
    (fmt.before.unwrap_or(0.0), rows, fmt.after.unwrap_or(0.0))
}

/// Paragraphs stacked in a column, as items and their total height.
fn stack(paras: &[Para], width: f32) -> (Vec<Item>, f32) {
    let mut items = Vec::new();
    let mut y = 0.0;
    for (i, para) in paras.iter().enumerate() {
        let (before, rows, after) = lay_para(para, width);
        if i > 0 {
            y += before;
        }
        for row in rows {
            items.extend(row.items.into_iter().map(|it| it.moved(0.0, y)));
            y += row.height;
        }
        if i + 1 < paras.len() {
            y += after;
        }
    }
    (items, y)
}

struct Pager {
    geo: Geometry,
    pages: Vec<Vec<Item>>,
    y: f32,
}

impl Pager {
    fn new(geo: Geometry) -> Pager {
        Pager {
            geo,
            pages: vec![Vec::new()],
            y: geo.top,
        }
    }

    fn page(&self) -> usize {
        self.pages.len() - 1
    }

    fn at_top(&self) -> bool {
        self.y <= self.geo.top + 0.01
    }

    fn bottom(&self) -> f32 {
        self.geo.height - self.geo.bottom
    }

    fn new_page(&mut self) {
        self.pages.push(Vec::new());
        self.y = self.geo.top;
    }

    fn put(&mut self, items: Vec<Item>, dx: f32) {
        let y = self.y;
        if let Some(page) = self.pages.last_mut() {
            page.extend(items.into_iter().map(|it| it.moved(dx, y)));
        }
    }

    fn para(&mut self, para: &Para) {
        let (before, rows, after) = lay_para(para, self.geo.text_width());
        if !self.at_top() {
            self.y += before;
        }
        // Keep a heading with the lines after it.
        let first = rows.first().map_or(0.0, |r| r.height);
        if para.heading.is_some() && !self.at_top() && self.y + first * 3.0 > self.bottom() {
            self.new_page();
        }
        for row in rows {
            if (row.page_break && !self.at_top())
                || (self.y + row.height > self.bottom() + 0.01 && !self.at_top())
            {
                self.new_page();
            }
            self.put(row.items, self.geo.left);
            self.y += row.height;
        }
        self.y += after;
    }

    fn table(&mut self, table: &Table) {
        let room = self.geo.text_width();
        let columns = table
            .rows
            .iter()
            .map(|r| r.iter().map(|c| c.span).sum::<usize>())
            .max()
            .unwrap_or(1)
            .max(table.grid.len())
            .max(1);
        let mut grid = table.grid.clone();
        grid.resize(columns, 0.0);
        let known: f32 = grid.iter().sum();
        if known <= 0.0 {
            grid = vec![room / columns as f32; columns];
        } else {
            let fill = grid.iter().filter(|w| **w <= 0.0).count();
            let spare = (room - known).max(0.0);
            for w in grid.iter_mut().filter(|w| **w <= 0.0) {
                *w = if fill > 0 { spare / fill as f32 } else { 0.0 };
            }
            let total: f32 = grid.iter().sum();
            if total > room {
                for w in &mut grid {
                    *w *= room / total;
                }
            }
        }
        let edges: Vec<f32> = std::iter::once(0.0)
            .chain(grid.iter().scan(0.0, |x, w| {
                *x += w;
                Some(*x)
            }))
            .collect();
        for row in &table.rows {
            let mut laid = Vec::new();
            let mut col = 0usize;
            let mut height: f32 = 0.0;
            for cell in row {
                let start = col.min(columns);
                let end = (col + cell.span).min(columns);
                col += cell.span;
                let (x0, x1) = (edges[start], edges[end.max(start)]);
                let inner = (x1 - x0 - 2.0 * CELL_PAD_X).max(1.0);
                let (items, h) = if cell.continued {
                    (Vec::new(), 0.0)
                } else {
                    stack(&cell.paras, inner)
                };
                height = height.max(h + 2.0 * CELL_PAD_Y);
                laid.push((x0, x1, items, cell));
            }
            if self.y + height > self.bottom() + 0.01 && !self.at_top() {
                self.new_page();
            }
            let mut items = Vec::new();
            for (x0, x1, cell_items, cell) in laid {
                if let Some(color) = cell.shade {
                    items.push(Item::Fill {
                        rect: Rect {
                            x: x0,
                            y: 0.0,
                            w: x1 - x0,
                            h: height,
                        },
                        color,
                    });
                }
                items.extend(
                    cell_items
                        .into_iter()
                        .map(|it| it.moved(x0 + CELL_PAD_X, CELL_PAD_Y)),
                );
                if table.borders {
                    if !cell.continued {
                        items.push(Item::Rule {
                            x1: x0,
                            y1: 0.0,
                            x2: x1,
                            y2: 0.0,
                        });
                    }
                    items.push(Item::Rule {
                        x1: x0,
                        y1: height,
                        x2: x1,
                        y2: height,
                    });
                    items.push(Item::Rule {
                        x1: x0,
                        y1: 0.0,
                        x2: x0,
                        y2: height,
                    });
                    items.push(Item::Rule {
                        x1,
                        y1: 0.0,
                        x2: x1,
                        y2: height,
                    });
                }
            }
            self.put(items, self.geo.left);
            self.y += height;
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown, for the terminal look.

fn inline_markdown(items: &[Inline], emphasis: bool) -> String {
    let mut out = String::new();
    for item in items {
        match item {
            Inline::Text(text, style) => {
                let marks = if emphasis {
                    format!(
                        "{}{}{}",
                        if style.strike { "~~" } else { "" },
                        if style.bold { "**" } else { "" },
                        if style.italic { "*" } else { "" }
                    )
                } else {
                    String::new()
                };
                let closing: String = marks.chars().rev().collect();
                let lines: Vec<String> = text
                    .replace(TAB, " ")
                    .split('\n')
                    .map(|line| {
                        let core = line.trim();
                        if core.is_empty() || marks.is_empty() {
                            return md_escape(line);
                        }
                        let lead = &line[..line.len() - line.trim_start().len()];
                        let trail = &line[line.trim_end().len()..];
                        format!("{lead}{marks}{}{closing}{trail}", md_escape(core))
                    })
                    .collect();
                out.push_str(&lines.join("  \n"));
            }
            Inline::Picture { part, .. } => {
                let _ = write!(out, "\n\n![]({})\n\n", Doc::image_href(&picture_id(part)));
            }
            Inline::PageBreak => {}
        }
    }
    out
}

fn para_markdown(para: &Para) -> String {
    if let Some(level) = para.heading {
        let text = inline_markdown(&para.items, false);
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }
        let hashes = "#".repeat((level as usize).clamp(1, 6));
        return format!("{hashes} {}", text.replace("  \n", " "));
    }
    let text = inline_markdown(&para.items, true);
    let text = text.trim();
    match &para.list {
        Some((mark, level)) => {
            let indent = "  ".repeat(*level as usize);
            let marker = match mark {
                Mark::Number(n)
                    if n.len() > 1
                        && n.ends_with(['.', ')'])
                        && n[..n.len() - 1].chars().all(|c| c.is_ascii_digit()) =>
                {
                    n.clone()
                }
                Mark::Number(n) => format!("- {}", md_escape(n)),
                Mark::Bullet(_) => "-".into(),
            };
            format!("{indent}{marker} {text}")
        }
        None => text.to_owned(),
    }
}

fn plain(paras: &[Para]) -> String {
    paras
        .iter()
        .map(|p| {
            inline_markdown(&p.items, true)
                .replace("  \n", " ")
                .trim()
                .to_owned()
        })
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn table_markdown(table: &Table) -> String {
    let rows: Vec<Vec<String>> = table
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|c| {
                    if c.continued {
                        String::new()
                    } else {
                        plain(&c.paras)
                    }
                })
                .collect()
        })
        .collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return String::new();
    }
    let line = |row: &Vec<String>| {
        let mut cells = row.clone();
        cells.resize(width, String::new());
        format!("| {} |", cells.join(" | "))
    };
    let mut md = line(&rows[0]);
    let _ = write!(md, "\n|{}", " --- |".repeat(width));
    for row in &rows[1..] {
        md.push('\n');
        md.push_str(&line(row));
    }
    md
}

fn block_markdown(block: &Block) -> String {
    match block {
        Block::Para(p) => para_markdown(p),
        Block::Table(t) => table_markdown(t),
        Block::PageBreak => String::new(),
    }
}

/// Markdown blocks joined, with list items kept together.
fn join_markdown(blocks: &[&Block]) -> String {
    let mut out = String::new();
    let mut last_list = false;
    for block in blocks {
        let md = block_markdown(block);
        if md.trim().is_empty() {
            continue;
        }
        let list = matches!(block, Block::Para(p) if p.list.is_some());
        if !out.is_empty() {
            out.push_str(if list && last_list { "\n" } else { "\n\n" });
        }
        out.push_str(&md);
        last_list = list;
    }
    out
}

fn picture_id(part: &str) -> String {
    format!("pic:{part}")
}

// ---------------------------------------------------------------------------
// The document as a source.

struct Words {
    package: Package,
    geo: Geometry,
    pages: Vec<Vec<Item>>,
    text: Vec<String>,
    look: Cell<Look>,
    pictures: RefCell<HashMap<String, (u32, u32)>>,
}

impl Words {
    fn render(&self, index: usize) -> Option<String> {
        let items = self.pages.get(index)?;
        let (w, h) = (self.geo.width, self.geo.height);
        let mut canvas = Canvas::default();
        let _ = write!(
            canvas.body,
            r##"<rect x="0" y="0" width="{w:.2}" height="{h:.2}" fill="#ffffff"/>"##
        );
        // Fills first, so text and rules sit on top of them.
        for item in items {
            if let Item::Fill { rect, color } = item {
                let _ = write!(
                    canvas.body,
                    r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{}"/>"#,
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    color.css()
                );
            }
        }
        for item in items {
            match item {
                Item::Text { line, x, baseline } => {
                    typeset::svg_line(&mut canvas.body, line, *x, *baseline)
                }
                Item::Picture { part, rect } => {
                    let Some(uri) = self.package.bytes(part).and_then(|b| data_uri(&b)) else {
                        let _ = write!(
                            canvas.body,
                            r##"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="#f2f2f2" stroke="#c8c8c8"/>"##,
                            rect.x, rect.y, rect.w, rect.h
                        );
                        continue;
                    };
                    let clip = format!("M0 0H{:.2}V{:.2}H0Z", rect.w, rect.h);
                    drawing::picture(&mut canvas, &uri, *rect, None, &clip);
                }
                Item::Rule { x1, y1, x2, y2 } => {
                    let _ = write!(
                        canvas.body,
                        r##"<line x1="{x1:.2}" y1="{y1:.2}" x2="{x2:.2}" y2="{y2:.2}" stroke="#000000" stroke-width="0.5"/>"##
                    );
                }
                Item::Fill { .. } => {}
            }
        }
        Some(canvas.finish(w, h))
    }
}

impl Source for Words {
    fn page(&self, index: usize) -> Rc<str> {
        match self.look.get() {
            Look::Original => format!("![]({})", Doc::image_href(&format!("page-{index}"))).into(),
            Look::Terminal => {
                let text = self.text.get(index).map(String::as_str).unwrap_or("");
                if text.trim().is_empty() {
                    "*This page has no text.*".into()
                } else {
                    text.into()
                }
            }
        }
    }

    fn image_size(&self, id: &str) -> Option<(u32, u32)> {
        if id.starts_with("page-") {
            let ratio = self.geo.height / self.geo.width.max(1.0);
            return Some((
                MEASURE_WIDTH,
                (MEASURE_WIDTH as f32 * ratio).round().max(1.0) as u32,
            ));
        }
        let part = id.strip_prefix("pic:")?;
        if let Some(size) = self.pictures.borrow().get(part) {
            return Some(*size);
        }
        let size = crate::office::picture_size(&self.package.bytes(part)?)?;
        self.pictures.borrow_mut().insert(part.to_owned(), size);
        Some(size)
    }

    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage> {
        if let Some(index) = id.strip_prefix("page-").and_then(|i| i.parse().ok()) {
            let svg = self.render(index)?;
            return typeset::rasterize(&svg, width.clamp(1, MAX_WIDTH));
        }
        crate::office::picture_pixels(&self.package.bytes(id.strip_prefix("pic:")?)?, width)
    }

    fn look(&self) -> Option<Look> {
        Some(self.look.get())
    }

    fn set_look(&self, look: Look) {
        self.look.set(look);
    }
}

pub fn open(path: &Path) -> Result<Doc, PackageFailed> {
    let package = Package::open(path, "Word")?;
    let invalid = |detail: String| PackageFailed::Invalid {
        path: path.display().to_string(),
        kind: "Word",
        detail,
    };
    let main = rel_by_kind(&package.rels(""), "officeDocument")
        .map(|r| r.target.clone())
        .unwrap_or_else(|| "word/document.xml".into());
    let xml = package
        .text(&main)
        .ok_or_else(|| invalid(format!("it has no {main}")))?;
    let doc =
        Document::parse(&xml).map_err(|e| invalid(format!("{main} isn't valid XML ({e})")))?;
    let body = doc
        .root_element()
        .child("body")
        .ok_or_else(|| invalid(format!("{main} has no body")))?;
    let rels = package.rels(&main);
    let theme = rel_by_kind(&rels, "theme")
        .and_then(|r| package.text(&r.target))
        .map(|x| Theme::parse(&x))
        .unwrap_or_else(|| Theme::parse(""));
    let styles = Styles::parse(
        rel_by_kind(&rels, "styles")
            .and_then(|r| package.text(&r.target))
            .as_deref(),
        &theme,
    );
    let numbering = Numbering::parse(
        rel_by_kind(&rels, "numbering")
            .and_then(|r| package.text(&r.target))
            .as_deref(),
    );
    let mut reader = Reader {
        styles: &styles,
        numbering,
        theme: &theme,
        rels: &rels,
        fields: Vec::new(),
    };
    let mut blocks = Vec::new();
    reader.blocks(body, &mut blocks);
    let geo = Geometry::parse(body.child("sectPr"));
    let (pages, text) = paginate(&blocks, geo);
    Ok(Doc {
        title: crate::office::stem(path),
        page_count: Some(pages.len()),
        source: Box::new(Words {
            package,
            geo,
            pages,
            text,
            look: Cell::new(Look::Original),
            pictures: RefCell::new(HashMap::new()),
        }),
    })
}

/// The blocks laid out on pages, and each page's markdown: the blocks that
/// start on it.
fn paginate(blocks: &[Block], geo: Geometry) -> (Vec<Vec<Item>>, Vec<String>) {
    let mut pager = Pager::new(geo);
    let mut starts = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Para(p) => {
                let breaks_first = p
                    .items
                    .first()
                    .is_some_and(|i| matches!(i, Inline::PageBreak));
                starts.push(pager.page() + usize::from(breaks_first && !pager.at_top()));
                pager.para(p);
            }
            Block::Table(t) => {
                starts.push(pager.page());
                pager.table(t);
            }
            Block::PageBreak => {
                if !pager.at_top() {
                    pager.new_page();
                }
                starts.push(pager.page());
            }
        }
    }
    let pages = pager.pages;
    let mut text = vec![Vec::new(); pages.len()];
    for (block, page) in blocks.iter().zip(starts) {
        if let Some(t) = text.get_mut(page.min(pages.len() - 1)) {
            t.push(block);
        }
    }
    (pages, text.iter().map(|b| join_markdown(b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::xml_escape;
    use std::io::Write;

    fn contains_text(svg: &str, text: &str) -> bool {
        svg.contains(&xml_escape(text))
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    fn docx(dir: &Path, body: &str, styles: &str, numbering: &str) -> std::path::PathBuf {
        let path = dir.join("doc.docx");
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let o = zip::write::SimpleFileOptions::default();
        let mut put = |name: &str, body: String| {
            zip.start_file(name, o).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        };
        put(
            "_rels/.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.into(),
        );
        put(
            "word/_rels/document.xml.rels",
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rS" Type="x/styles" Target="styles.xml"/><Relationship Id="rN" Type="x/numbering" Target="numbering.xml"/></Relationships>"#.into(),
        );
        put(
            "word/document.xml",
            format!(
                r#"<w:document {W}><w:body>{body}<w:sectPr><w:pgSz w:w="12240" w:h="15840"/><w:pgMar w:top="1440" w:right="1440" w:bottom="1440" w:left="1440"/></w:sectPr></w:body></w:document>"#
            ),
        );
        put(
            "word/styles.xml",
            format!(r#"<w:styles {W}>{styles}</w:styles>"#),
        );
        put(
            "word/numbering.xml",
            format!(r#"<w:numbering {W}>{numbering}</w:numbering>"#),
        );
        zip.finish().unwrap();
        path
    }

    fn tempdir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-word-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn para(text: &str) -> String {
        format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    const HEADING_STYLE: &str = r#"<w:docDefaults><w:rPrDefault><w:rPr><w:sz w:val="22"/></w:rPr></w:rPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:rPr><w:b/><w:sz w:val="32"/></w:rPr></w:style>"#;

    #[test]
    fn reads_headings_emphasis_and_fields_as_markdown() {
        let dir = tempdir("md");
        let body = format!(
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>Intro</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Plain </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>bold</w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r><w:r><w:instrText>PAGE</w:instrText></w:r><w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t xml:space="preserve"> 7</w:t></w:r><w:r><w:fldChar w:fldCharType="end"/></w:r></w:p>{}"#,
            para("a_b")
        );
        let doc = open(&docx(&dir, &body, HEADING_STYLE, "")).unwrap();
        doc.source.set_look(Look::Terminal);
        assert_eq!(&*doc.source.page(0), "# Intro\n\nPlain **bold** 7\n\na\\_b");
    }

    #[test]
    fn numbers_list_items_and_restarts_deeper_levels() {
        let dir = tempdir("list");
        let numbering = r#"<w:abstractNum w:abstractNumId="0"><w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl><w:lvl w:ilvl="1"><w:start w:val="1"/><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl></w:abstractNum><w:abstractNum w:abstractNumId="1"><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#xF0B7;"/></w:lvl></w:abstractNum><w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num><w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>"#;
        let item = |id: u8, level: u8, text: &str| {
            format!(
                r#"<w:p><w:pPr><w:numPr><w:ilvl w:val="{level}"/><w:numId w:val="{id}"/></w:numPr></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
            )
        };
        let body = [
            item(1, 0, "one"),
            item(1, 1, "sub"),
            item(1, 0, "two"),
            item(1, 1, "again"),
            item(2, 0, "dot"),
        ]
        .concat();
        let doc = open(&docx(&dir, &body, "", numbering)).unwrap();
        doc.source.set_look(Look::Terminal);
        assert_eq!(
            &*doc.source.page(0),
            "1. one\n  - a) sub\n2. two\n  - a) again\n- dot"
        );
    }

    #[test]
    fn tables_read_as_markdown_and_draw_borders() {
        let dir = tempdir("table");
        let cell = |t: &str| format!("<w:tc>{}</w:tc>", para(t));
        let body = format!(
            r#"<w:tbl><w:tblPr><w:tblBorders><w:top w:val="single"/></w:tblBorders></w:tblPr><w:tblGrid><w:gridCol w:w="2000"/><w:gridCol w:w="2000"/></w:tblGrid><w:tr>{}{}</w:tr><w:tr>{}{}</w:tr></w:tbl>"#,
            cell("Name"),
            cell("Score"),
            cell("Ada"),
            cell("42")
        );
        let doc = open(&docx(&dir, &body, "", "")).unwrap();
        doc.source.set_look(Look::Terminal);
        assert_eq!(
            &*doc.source.page(0),
            "| Name | Score |\n| --- | --- |\n| Ada | 42 |"
        );
        doc.source.set_look(Look::Original);
        assert_eq!(&*doc.source.page(0), "![](leaf-image:page-0)");
        let image = doc.source.decode("page-0", 400).unwrap();
        assert_eq!(image.width(), 400);
        assert!(image.pixels().any(|p| p.0[0] < 100));
    }

    #[test]
    fn long_documents_break_onto_pages_that_both_looks_share() {
        let dir = tempdir("pages");
        let mut body: String = (0..120)
            .map(|i| para(&format!("Paragraph number {i}")))
            .collect();
        body.push_str(r#"<w:p><w:r><w:br w:type="page"/><w:t>After the break</w:t></w:r></w:p>"#);
        let doc = open(&docx(&dir, &body, HEADING_STYLE, "")).unwrap();
        let pages = doc.page_count.unwrap();
        assert!(pages >= 3, "{pages} pages");
        doc.source.set_look(Look::Terminal);
        assert!(doc.source.page(0).starts_with("Paragraph number 0"));
        assert!(!doc.source.page(1).contains("Paragraph number 0\n"));
        assert_eq!(&*doc.source.page(pages - 1), "After the break");
        let lines: Vec<String> = (0..pages)
            .flat_map(|i| {
                doc.source
                    .page(i)
                    .split("\n\n")
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<String> = (0..120)
            .map(|i| format!("Paragraph number {i}"))
            .chain(["After the break".to_owned()])
            .collect();
        assert_eq!(lines, expected);
    }

    #[test]
    fn draws_text_in_the_document_font() {
        let dir = tempdir("draw");
        let body = r#"<w:p><w:pPr><w:jc w:val="center"/></w:pPr><w:r><w:rPr><w:rFonts w:ascii="Times New Roman"/><w:color w:val="FF0000"/></w:rPr><w:t>Hello &amp; welcome</w:t></w:r></w:p>"#;
        let doc = open(&docx(&dir, body, HEADING_STYLE, "")).unwrap();
        let image = doc.source.decode("page-0", 600).unwrap();
        assert!(
            image
                .pixels()
                .any(|p| p.0[0] > 200 && p.0[1] < 80 && p.0[2] < 80)
        );
    }

    #[test]
    fn bullets_from_symbol_fonts_become_plain_bullets() {
        assert_eq!(bullet("\u{f0b7}"), "•");
        assert_eq!(bullet("o"), "◦");
        assert_eq!(bullet("\u{f0a7}"), "▪");
        assert_eq!(number("upperRoman", 4), "IV");
        assert_eq!(number("lowerLetter", 28), "ab");
    }

    #[test]
    fn rendered_pages_hold_their_words() {
        let dir = tempdir("svg");
        let path = docx(&dir, &para("Fish &amp; chips"), HEADING_STYLE, "");
        let package = Package::open(&path, "Word").unwrap();
        let xml = package.text("word/document.xml").unwrap();
        let parsed = Document::parse(&xml).unwrap();
        let body = parsed.root_element().child("body").unwrap();
        let theme = Theme::parse("");
        let styles = Styles::parse(None, &theme);
        let mut reader = Reader {
            styles: &styles,
            numbering: Numbering::default(),
            theme: &theme,
            rels: &[],
            fields: Vec::new(),
        };
        let mut blocks = Vec::new();
        reader.blocks(body, &mut blocks);
        let geo = Geometry::parse(body.child("sectPr"));
        let (pages, _) = paginate(&blocks, geo);
        let words = Words {
            package,
            geo,
            pages,
            text: Vec::new(),
            look: Cell::new(Look::Original),
            pictures: RefCell::new(HashMap::new()),
        };
        assert!(contains_text(&words.render(0).unwrap(), "Fish & chips"));
    }
}
