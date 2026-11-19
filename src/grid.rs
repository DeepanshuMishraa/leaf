//! A page of a spreadsheet drawn as a spreadsheet draws it: column letters
//! and row numbers, gridlines, and each cell in its own font, fill, border
//! and alignment. Excel workbooks (`.xlsx`, `.xlsm`) bring their cell
//! styles, column widths and row heights; other sheets get a plain grid
//! sized to fit their cells.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;
use std::rc::Rc;

use crate::ooxml::{Color, Local, Package, Theme, rel_by_id, rel_by_kind, roxmltree};
use crate::typeset::{self, Run, Style};
use roxmltree::Document;

/// Furthest text in a page's last column runs on past it, in points.
const MAX_SPILL: f32 = 300.0;
/// The column letters' band, in points.
const HEADER_HEIGHT: f32 = 18.0;
/// A row with nothing to say about its height.
const ROW_HEIGHT: f32 = 15.0;
/// Excel's standard column (8.43 characters).
const COLUMN_WIDTH: f32 = 48.0;
const GRIDLINE: &str = "#d9d9d9";
const HEADER_FILL: &str = "#f3f3f3";
const HEADER_TEXT: Color = Color::rgb(0x59, 0x59, 0x59);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Borders {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

/// How a cell looks.
#[derive(Debug, Clone, PartialEq)]
pub struct CellLook {
    font: Rc<Style>,
    fill: Option<Color>,
    align: Option<HAlign>,
    borders: Borders,
}

impl Default for CellLook {
    fn default() -> Self {
        CellLook {
            font: Rc::new(Style {
                family: "Calibri".into(),
                size: 11.0,
                ..Style::default()
            }),
            fill: None,
            align: None,
            borders: Borders::default(),
        }
    }
}

/// A worksheet's styled cells and sizes, by absolute row and column.
#[derive(Debug, Default)]
pub struct SheetLooks {
    cells: HashMap<(usize, usize), usize>,
    widths: HashMap<usize, f32>,
    heights: HashMap<usize, f32>,
    default_width: Option<f32>,
    default_height: Option<f32>,
}

/// A workbook's cell styles and each worksheet's use of them.
#[derive(Debug, Default)]
pub struct Looks {
    styles: Vec<CellLook>,
    sheets: HashMap<String, SheetLooks>,
}

impl Looks {
    /// The looks of an Excel workbook, or `None` for other files or one
    /// whose styles can't be read (its cells still show, plainly).
    pub fn read(path: &Path) -> Option<Looks> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        if !matches!(ext.as_str(), "xlsx" | "xlsm" | "xltx" | "xltm") {
            return None;
        }
        let package = Package::open(path, "Excel").ok()?;
        let book = rel_by_kind(&package.rels(""), "officeDocument")
            .map(|r| r.target.clone())
            .unwrap_or_else(|| "xl/workbook.xml".into());
        let rels = package.rels(&book);
        let theme = rel_by_kind(&rels, "theme")
            .and_then(|r| package.text(&r.target))
            .map(|x| Theme::parse(&x))
            .unwrap_or_default();
        let styles = rel_by_kind(&rels, "styles")
            .and_then(|r| package.text(&r.target))
            .map(|x| parse_styles(&x, &theme))
            .unwrap_or_default();
        let xml = package.text(&book)?;
        let doc = Document::parse(&xml).ok()?;
        let mut sheets = HashMap::new();
        for sheet in doc.descendants().filter(|n| n.has_tag_name_local("sheet")) {
            let Some(name) = sheet.attribute("name") else {
                continue;
            };
            let Some(rel) = sheet
                .attributes()
                .find(|a| a.name() == "id" && a.namespace().is_some())
                .and_then(|a| rel_by_id(&rels, a.value()))
            else {
                continue;
            };
            if let Some(xml) = package.text(&rel.target) {
                sheets.insert(name.to_owned(), parse_sheet(&xml));
            }
        }
        Some(Looks { styles, sheets })
    }

    pub fn sheet(&self, name: &str) -> Option<&SheetLooks> {
        self.sheets.get(name)
    }

    fn cell(&self, sheet: Option<&SheetLooks>, at: (usize, usize)) -> Option<&CellLook> {
        let index = sheet?.cells.get(&at)?;
        self.styles.get(*index)
    }
}

/// An Excel color element: `rgb`, `theme` with `tint`, or `indexed`.
fn excel_color(node: roxmltree::Node, theme: &Theme) -> Option<Color> {
    if let Some(rgb) = node.attribute("rgb") {
        return Color::hex(&rgb[rgb.len().saturating_sub(6)..]);
    }
    let tint = node
        .attribute("tint")
        .and_then(|t| t.parse::<f32>().ok())
        .unwrap_or(0.0);
    if let Some(index) = node
        .attribute("theme")
        .and_then(|t| t.parse::<usize>().ok())
    {
        const SLOTS: [&str; 12] = [
            "lt1", "dk1", "lt2", "dk2", "accent1", "accent2", "accent3", "accent4", "accent5",
            "accent6", "hlink", "folHlink",
        ];
        let color = theme.scheme(SLOTS.get(index)?)?;
        return Some(if tint == 0.0 {
            color
        } else {
            color.tinted(tint)
        });
    }
    let indexed = node.attribute("indexed")?.parse::<usize>().ok()?;
    Some(match indexed {
        0 | 8 | 64 => Color::BLACK,
        1 | 9 | 65 => Color::WHITE,
        2 | 10 => Color::rgb(255, 0, 0),
        3 | 11 => Color::rgb(0, 255, 0),
        4 | 12 => Color::rgb(0, 0, 255),
        5 | 13 => Color::rgb(255, 255, 0),
        6 | 14 => Color::rgb(255, 0, 255),
        7 | 15 => Color::rgb(0, 255, 255),
        _ => return None,
    })
}

fn parse_styles(xml: &str, theme: &Theme) -> Vec<CellLook> {
    let Ok(doc) = Document::parse(xml) else {
        return Vec::new();
    };
    let root = doc.root_element();
    let flag = |n: roxmltree::Node, name: &str| {
        n.child(name)
            .is_some_and(|f| !matches!(f.attribute("val"), Some("0" | "false" | "none")))
    };
    let fonts: Vec<Style> = root
        .child("fonts")
        .map(|f| {
            f.kids("font")
                .map(|font| Style {
                    family: font
                        .child("name")
                        .and_then(|n| n.attribute("val"))
                        .unwrap_or("Calibri")
                        .to_owned(),
                    size: font
                        .child("sz")
                        .and_then(|n| n.attribute("val"))
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(11.0),
                    bold: flag(font, "b"),
                    italic: flag(font, "i"),
                    underline: flag(font, "u"),
                    strike: flag(font, "strike"),
                    color: font
                        .child("color")
                        .and_then(|c| excel_color(c, theme))
                        .unwrap_or(Color::BLACK),
                    ..Style::default()
                })
                .collect()
        })
        .unwrap_or_default();
    let fills: Vec<Option<Color>> = root
        .child("fills")
        .map(|f| {
            f.kids("fill")
                .map(|fill| {
                    let pattern = fill.child("patternFill")?;
                    if pattern.attribute("patternType") != Some("solid") {
                        return None;
                    }
                    excel_color(pattern.child("fgColor")?, theme)
                })
                .collect()
        })
        .unwrap_or_default();
    let borders: Vec<Borders> = root
        .child("borders")
        .map(|b| {
            b.kids("border")
                .map(|border| {
                    let side = |name: &str| {
                        border
                            .child(name)
                            .and_then(|s| s.attribute("style"))
                            .is_some_and(|s| s != "none")
                    };
                    Borders {
                        left: side("left") || side("start"),
                        right: side("right") || side("end"),
                        top: side("top"),
                        bottom: side("bottom"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let Some(xfs) = root.child("cellXfs") else {
        return Vec::new();
    };
    let index = |n: roxmltree::Node, name: &str| {
        n.attribute(name)
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
    };
    xfs.kids("xf")
        .map(|xf| CellLook {
            font: Rc::new(
                fonts
                    .get(index(xf, "fontId"))
                    .cloned()
                    .unwrap_or_else(|| (*CellLook::default().font).clone()),
            ),
            fill: fills.get(index(xf, "fillId")).copied().flatten(),
            align: xf
                .child("alignment")
                .and_then(|a| a.attribute("horizontal"))
                .and_then(|h| match h {
                    "left" => Some(HAlign::Left),
                    "center" | "centerContinuous" => Some(HAlign::Center),
                    "right" => Some(HAlign::Right),
                    _ => None,
                }),
            borders: borders
                .get(index(xf, "borderId"))
                .copied()
                .unwrap_or_default(),
        })
        .collect()
}

/// Excel's column width, in characters of its default font, as points.
fn width_points(chars: f32) -> f32 {
    ((chars * 7.0 + 5.0).floor()) * 0.75
}

/// `B3` as (row 2, column 1).
fn cell_ref(text: &str) -> Option<(usize, usize)> {
    let split = text.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = text.split_at(split);
    let mut col = 0usize;
    for c in letters.chars() {
        if !c.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + (c.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    let row: usize = digits.parse().ok()?;
    Some((row.checked_sub(1)?, col.checked_sub(1)?))
}

fn parse_sheet(xml: &str) -> SheetLooks {
    let mut looks = SheetLooks::default();
    let Ok(doc) = Document::parse(xml) else {
        return looks;
    };
    let root = doc.root_element();
    if let Some(format) = root.child("sheetFormatPr") {
        looks.default_width = format
            .attribute("defaultColWidth")
            .and_then(|w| w.parse().ok())
            .map(width_points);
        looks.default_height = format
            .attribute("defaultRowHeight")
            .and_then(|h| h.parse().ok());
    }
    if let Some(cols) = root.child("cols") {
        for col in cols.kids("col") {
            let get = |n: &str| col.attribute(n).and_then(|v| v.parse::<usize>().ok());
            let (Some(min), Some(max), Some(width)) = (
                get("min"),
                get("max"),
                col.attribute("width").and_then(|w| w.parse::<f32>().ok()),
            ) else {
                continue;
            };
            let hidden = matches!(col.attribute("hidden"), Some("1" | "true"));
            // Ranges such as min=1 max=16384 style a whole sheet; stop at a sane edge.
            for c in min..=max.min(min + 1024) {
                looks
                    .widths
                    .insert(c - 1, if hidden { 0.0 } else { width_points(width) });
            }
        }
    }
    if let Some(data) = root.child("sheetData") {
        for row in data.kids("row") {
            let r = row
                .attribute("r")
                .and_then(|v| v.parse::<usize>().ok())
                .and_then(|r| r.checked_sub(1));
            if let (Some(r), Some(ht)) = (r, row.attribute("ht").and_then(|h| h.parse().ok())) {
                looks.heights.insert(r, ht);
            }
            for c in row.kids("c") {
                if let (Some(at), Some(s)) = (
                    c.attribute("r").and_then(cell_ref),
                    c.attribute("s").and_then(|s| s.parse().ok()),
                ) {
                    looks.cells.insert(at, s);
                }
            }
        }
    }
    looks
}

/// Whether a cell reads as a number or date, which a spreadsheet right-aligns.
fn numeric(text: &str) -> bool {
    let t = text.trim().trim_end_matches('%').replace(',', "");
    !t.is_empty()
        && (t.parse::<f64>().is_ok()
            || (t.len() >= 10
                && t.as_bytes()[4] == b'-'
                && t.as_bytes()[7] == b'-'
                && t[..4].chars().all(|c| c.is_ascii_digit())))
}

/// One line of `text` in `style`, cut to fit `room` points.
fn fitted(text: &str, style: &Rc<Style>, room: f32) -> Option<typeset::Line> {
    let mut text: String = text.replace(['\n', '\r', '\t'], " ").trim().to_owned();
    while !text.is_empty() && typeset::measure(&text, style) > room {
        text.pop();
    }
    if text.is_empty() {
        return None;
    }
    let run = Run {
        text,
        style: style.clone(),
    };
    typeset::wrap(&[run], f32::MAX, f32::MAX, style)
        .into_iter()
        .next()
}

/// A page of cells to draw.
pub struct Grid<'a> {
    /// Each row's number in the sheet (from 0) and its cells.
    pub rows: Vec<(usize, &'a [String])>,
    /// Each column's number in the sheet (from 0) and its index in a row.
    pub columns: Vec<(usize, usize)>,
    pub looks: Option<&'a Looks>,
    pub sheet: Option<&'a SheetLooks>,
}

impl Grid<'_> {
    fn look(&self, at: (usize, usize)) -> CellLook {
        self.looks
            .and_then(|l| l.cell(self.sheet, at))
            .cloned()
            .unwrap_or_default()
    }

    fn text(&self, row: usize, col: usize) -> &str {
        self.rows[row]
            .1
            .get(self.columns[col].1)
            .map_or("", String::as_str)
    }

    /// Column widths: the workbook's, or wide enough for the cells shown.
    fn widths(&self) -> Vec<f32> {
        self.columns
            .iter()
            .enumerate()
            .map(|(i, (abs, _))| {
                if let Some(sheet) = self.sheet {
                    return sheet
                        .widths
                        .get(abs)
                        .copied()
                        .or(sheet.default_width)
                        .unwrap_or(COLUMN_WIDTH);
                }
                let widest = (0..self.rows.len())
                    .map(|r| {
                        let look = self.look((self.rows[r].0, *abs));
                        typeset::measure(self.text(r, i).trim(), &look.font)
                    })
                    .fold(0.0f32, f32::max);
                (widest + 10.0).clamp(COLUMN_WIDTH, 260.0)
            })
            .collect()
    }

    fn heights(&self) -> Vec<f32> {
        self.rows
            .iter()
            .map(|(abs, _)| {
                if let Some(h) = self.sheet.and_then(|s| s.heights.get(abs)) {
                    return *h;
                }
                let tallest = self
                    .columns
                    .iter()
                    .map(|(c, _)| self.look((*abs, *c)).font.size)
                    .fold(0.0f32, f32::max);
                self.sheet
                    .and_then(|s| s.default_height)
                    .unwrap_or(ROW_HEIGHT)
                    .max(tallest * 1.36)
            })
            .collect()
    }

    fn gutter_style() -> Rc<Style> {
        Rc::new(Style {
            family: "Calibri".into(),
            size: 10.0,
            color: HEADER_TEXT,
            ..Style::default()
        })
    }

    /// The row numbers' column, in points.
    fn gutter(&self, style: &Style) -> f32 {
        let last_row = self.rows.last().map_or(1, |(r, _)| r + 1);
        (typeset::measure(&last_row.to_string(), style) + 14.0).max(28.0)
    }

    /// Room past the last column for left-aligned text that runs on, as
    /// a spreadsheet lets it run into the empty columns beyond the page.
    fn spill(&self, widths: &[f32]) -> f32 {
        let (Some(last), Some(&room)) = (self.columns.len().checked_sub(1), widths.last()) else {
            return 0.0;
        };
        (0..self.rows.len())
            .map(|r| {
                let text = self.text(r, last).trim();
                let look = self.look((self.rows[r].0, self.columns[last].0));
                let left = look.align.unwrap_or(if numeric(text) {
                    HAlign::Right
                } else {
                    HAlign::Left
                }) == HAlign::Left;
                if text.is_empty() || !left {
                    return 0.0;
                }
                typeset::measure(text, &look.font) + 8.0 - room
            })
            .fold(0.0f32, f32::max)
            .min(MAX_SPILL)
    }

    /// The page's size in points.
    pub fn size(&self) -> (f32, f32) {
        let widths = self.widths();
        let w = self.gutter(&Self::gutter_style()) + widths.iter().sum::<f32>();
        let h = HEADER_HEIGHT + self.heights().iter().sum::<f32>();
        (w + self.spill(&widths), h)
    }

    /// The page as SVG.
    pub fn svg(&self) -> String {
        let widths = self.widths();
        let heights = self.heights();
        let gutter_style = Self::gutter_style();
        let gutter = self.gutter(&gutter_style);
        let mut xs = vec![gutter];
        for w in &widths {
            xs.push(xs[xs.len() - 1] + w);
        }
        let mut ys = vec![HEADER_HEIGHT];
        for h in &heights {
            ys.push(ys[ys.len() - 1] + h);
        }
        let content_w = xs[xs.len() - 1];
        let content_h = ys[ys.len() - 1];
        let width = content_w + self.spill(&widths);
        let height = content_h;
        let mut svg = String::new();
        let _ = write!(
            svg,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width:.2}" height="{height:.2}" viewBox="0 0 {width:.2} {height:.2}"><rect width="{width:.2}" height="{height:.2}" fill="#ffffff"/>"##
        );
        // Headers.
        let _ = write!(
            svg,
            r#"<rect x="0" y="0" width="{content_w:.2}" height="{HEADER_HEIGHT}" fill="{HEADER_FILL}"/><rect x="0" y="0" width="{gutter:.2}" height="{content_h:.2}" fill="{HEADER_FILL}"/>"#
        );
        let centered = |svg: &mut String, text: &str, x0: f32, x1: f32, y0: f32, y1: f32| {
            if let Some(line) = fitted(text, &gutter_style, x1 - x0 - 2.0) {
                let x = x0 + ((x1 - x0) - line.width) / 2.0;
                let y = y0 + (y1 - y0 + line.ascent - line.descent) / 2.0;
                typeset::svg_line(svg, &line, x, y);
            }
        };
        for (i, (abs, _)) in self.columns.iter().enumerate() {
            centered(
                &mut svg,
                &letters(*abs),
                xs[i],
                xs[i + 1],
                0.0,
                HEADER_HEIGHT,
            );
        }
        for (i, (abs, _)) in self.rows.iter().enumerate() {
            centered(
                &mut svg,
                &(abs + 1).to_string(),
                0.0,
                gutter,
                ys[i],
                ys[i + 1],
            );
        }
        // Fills.
        for (r, (row, _)) in self.rows.iter().enumerate() {
            for (c, (col, _)) in self.columns.iter().enumerate() {
                if let Some(fill) = self.look((*row, *col)).fill {
                    let _ = write!(
                        svg,
                        r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{}"/>"#,
                        xs[c],
                        ys[r],
                        xs[c + 1] - xs[c],
                        ys[r + 1] - ys[r],
                        fill.css()
                    );
                }
            }
        }
        // Gridlines.
        let mut lines = String::new();
        for x in &xs {
            let _ = write!(lines, "M{x:.2} 0V{content_h:.2}");
        }
        for y in &ys {
            let _ = write!(lines, "M0 {y:.2}H{content_w:.2}");
        }
        let _ = write!(
            svg,
            r#"<path d="{lines}M0 0H{content_w:.2}M0 0V{content_h:.2}" stroke="{GRIDLINE}" stroke-width="0.75" fill="none"/>"#
        );
        // Text, and borders over the gridlines.
        let mut borders = String::new();
        for (r, (row, _)) in self.rows.iter().enumerate() {
            for (c, (col, _)) in self.columns.iter().enumerate() {
                let look = self.look((*row, *col));
                let (x0, x1, y0, y1) = (xs[c], xs[c + 1], ys[r], ys[r + 1]);
                let b = look.borders;
                if b.left {
                    let _ = write!(borders, "M{x0:.2} {y0:.2}V{y1:.2}");
                }
                if b.right {
                    let _ = write!(borders, "M{x1:.2} {y0:.2}V{y1:.2}");
                }
                if b.top {
                    let _ = write!(borders, "M{x0:.2} {y0:.2}H{x1:.2}");
                }
                if b.bottom {
                    let _ = write!(borders, "M{x0:.2} {y1:.2}H{x1:.2}");
                }
                let text = self.text(r, c);
                if text.trim().is_empty() {
                    continue;
                }
                let align = look.align.unwrap_or(if numeric(text) {
                    HAlign::Right
                } else {
                    HAlign::Left
                });
                // Left-aligned text runs on over empty cells to its right.
                let mut reach = x1;
                if align == HAlign::Left {
                    let mut next = c + 1;
                    while next < self.columns.len() && self.text(r, next).trim().is_empty() {
                        reach = xs[next + 1];
                        next += 1;
                    }
                    if next == self.columns.len() {
                        reach = width;
                    }
                }
                let pad = 3.0;
                let Some(line) = fitted(text, &look.font, reach - x0 - 2.0 * pad) else {
                    continue;
                };
                let x = match align {
                    HAlign::Left => x0 + pad,
                    HAlign::Center => x0 + ((x1 - x0) - line.width) / 2.0,
                    HAlign::Right => x1 - pad - line.width,
                };
                // Spilled text hides the gridlines it crosses.
                let end = x + line.width;
                if end > x1 {
                    let _ = write!(
                        svg,
                        r##"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="#ffffff"/>"##,
                        x1 - 0.5,
                        y0 + 0.5,
                        end - x1 + 1.0,
                        y1 - y0 - 1.0
                    );
                }
                let y = y1 - line.descent - 2.0;
                typeset::svg_line(&mut svg, &line, x, y);
            }
        }
        if !borders.is_empty() {
            let _ = write!(
                svg,
                r##"<path d="{borders}" stroke="#000000" stroke-width="0.75" fill="none"/>"##
            );
        }
        svg.push_str("</svg>");
        svg
    }
}

/// A column's letters: 0 is A, 26 is AA.
pub fn letters(mut column: usize) -> String {
    let mut out = Vec::new();
    loop {
        out.push(b'A' + (column % 26) as u8);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_references_name_rows_and_columns_from_zero() {
        assert_eq!(cell_ref("A1"), Some((0, 0)));
        assert_eq!(cell_ref("B3"), Some((2, 1)));
        assert_eq!(cell_ref("AA10"), Some((9, 26)));
        assert_eq!(cell_ref("10"), None);
    }

    #[test]
    fn numbers_and_dates_align_right() {
        assert!(numeric("42"));
        assert!(numeric("1,234.5"));
        assert!(numeric("12%"));
        assert!(numeric("2023-03-15 12:00"));
        assert!(!numeric("Ada"));
        assert!(!numeric(""));
    }

    #[test]
    fn excel_styles_give_fonts_fills_and_borders() {
        let looks = parse_styles(
            r#"<styleSheet xmlns="x"><fonts><font><sz val="11"/><name val="Calibri"/></font><font><b/><sz val="14"/><color rgb="FFFF0000"/><name val="Arial"/></font></fonts><fills><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill><fill><patternFill patternType="solid"><fgColor rgb="FF00FF00"/></patternFill></fill></fills><borders><border/><border><left style="thin"/><bottom style="thin"/></border></borders><cellXfs><xf fontId="0" fillId="0" borderId="0"/><xf fontId="1" fillId="2" borderId="1"><alignment horizontal="center"/></xf></cellXfs></styleSheet>"#,
            &Theme::default(),
        );
        assert_eq!(looks.len(), 2);
        let styled = &looks[1];
        assert!(styled.font.bold);
        assert_eq!(styled.font.family, "Arial");
        assert_eq!(styled.font.color, Color::rgb(255, 0, 0));
        assert_eq!(styled.fill, Some(Color::rgb(0, 255, 0)));
        assert_eq!(styled.align, Some(HAlign::Center));
        assert!(styled.borders.left && styled.borders.bottom && !styled.borders.top);
        assert_eq!(looks[0].fill, None);
    }

    #[test]
    fn sheets_bring_widths_heights_and_cell_styles() {
        let looks = parse_sheet(
            r#"<worksheet xmlns="x"><sheetFormatPr defaultRowHeight="15"/><cols><col min="2" max="3" width="20" customWidth="1"/></cols><sheetData><row r="2" ht="30"><c r="B2" s="1"><v>1</v></c></row></sheetData></worksheet>"#,
        );
        assert_eq!(looks.widths.get(&1), Some(&width_points(20.0)));
        assert_eq!(looks.widths.get(&2), Some(&width_points(20.0)));
        assert_eq!(looks.heights.get(&1), Some(&30.0));
        assert_eq!(looks.cells.get(&(1, 1)), Some(&1));
        assert_eq!(width_points(8.43), 48.0);
    }

    #[test]
    fn draws_a_grid_with_its_cells() {
        let head = vec!["Name".to_owned(), "Score".to_owned()];
        let row = vec!["Ada".to_owned(), "42".to_owned()];
        let grid = Grid {
            rows: vec![(0, &head), (1, &row)],
            columns: vec![(0, 0), (1, 1)],
            looks: None,
            sheet: None,
        };
        let svg = grid.svg();
        assert!(svg.contains(">Ada<") && svg.contains(">42<") && svg.contains(">B<"));
        let image = typeset::rasterize(&svg, 600).unwrap();
        assert_eq!(image.width(), 600);
        assert!(image.pixels().any(|p| p.0[0] < 80));
    }
}
