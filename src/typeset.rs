//! Text set the way an office document sets it: in the document's own fonts
//! (or a metric-compatible stand-in when one isn't installed), measured,
//! wrapped into lines, and written out as SVG for resvg to draw.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use image::RgbaImage;
use resvg::usvg::fontdb::{self, Family, Query, Stretch, Weight};
use resvg::{tiny_skia, usvg};
use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::{GlyphMetrics, Metrics};
use skrifa::{FontRef, MetadataProvider};

use crate::ooxml::{Color, xml_escape};

/// How a run of text looks.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// The typeface the document asks for.
    pub family: String,
    /// Size in points.
    pub size: f32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub color: Color,
    /// Raised (positive) or lowered share of the size, for superscript and subscript.
    pub baseline: f32,
    /// Letters drawn as capitals.
    pub caps: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            family: "Calibri".into(),
            size: 11.0,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            color: Color::BLACK,
            baseline: 0.0,
            caps: false,
        }
    }
}

/// A font that is installed, picked for a style.
#[derive(Debug, Clone)]
struct Face {
    id: fontdb::ID,
    /// The installed family's name, for SVG, so resvg draws with the font measured.
    family: String,
    /// Ascent and descent per em, and the gap between lines.
    ascent: f32,
    descent: f32,
    leading: f32,
    /// How much narrower the asked-for font is than the one drawn: text is
    /// measured at the asked-for width and squeezed to fit, so lines break
    /// where the author saw them break.
    squeeze: f32,
}

/// Fonts whose letters are as wide as each other's, so one stands in for
/// another without changing where lines break.
const SAME_WIDTHS: [&[&str]; 6] = [
    &["calibri", "carlito"],
    &["cambria", "caladea"],
    &[
        "arial",
        "helvetica",
        "liberation sans",
        "arimo",
        "nimbus sans",
    ],
    &[
        "times new roman",
        "times",
        "liberation serif",
        "tinos",
        "nimbus roman",
    ],
    &[
        "courier new",
        "courier",
        "liberation mono",
        "cousine",
        "nimbus mono ps",
    ],
    &["georgia", "gelasio"],
];

/// The asked-for font's width as a share of the stand-in's (usually a
/// Helvetica-like sans), for common office fonts that are often missing.
fn squeeze(asked: &str, drawn: &str) -> f32 {
    let (asked, drawn) = (asked.to_ascii_lowercase(), drawn.to_ascii_lowercase());
    if asked == drawn
        || SAME_WIDTHS
            .iter()
            .any(|group| group.contains(&asked.as_str()) && group.contains(&drawn.as_str()))
    {
        return 1.0;
    }
    match asked.as_str() {
        "calibri" | "calibri light" | "carlito" => 0.89,
        "aptos" | "aptos display" => 0.95,
        "aptos narrow" | "arial narrow" => 0.82,
        "candara" | "corbel" => 0.9,
        "century gothic" => 1.1,
        _ => 1.0,
    }
}

/// Families that keep a missing font's letter widths, so lines break where
/// the document's author saw them break.
fn stand_ins(family: &str) -> &'static [&'static str] {
    match family.to_ascii_lowercase().as_str() {
        "calibri" | "calibri light" | "aptos" | "aptos display" | "aptos narrow" => &[
            "Carlito",
            "Calibri",
            "Helvetica Neue",
            "Arial",
            "Liberation Sans",
        ],
        "cambria" | "cambria math" => &["Caladea", "Cambria", "Georgia", "Liberation Serif"],
        "arial" | "helvetica" | "arial nova" => &[
            "Arial",
            "Helvetica",
            "Liberation Sans",
            "Arimo",
            "Nimbus Sans",
        ],
        "times new roman" | "times" => &[
            "Times New Roman",
            "Times",
            "Liberation Serif",
            "Tinos",
            "Nimbus Roman",
        ],
        "courier new" | "courier" | "consolas" => &[
            "Courier New",
            "Menlo",
            "Liberation Mono",
            "Cousine",
            "DejaVu Sans Mono",
        ],
        "segoe ui" | "segoe ui light" | "segoe ui semibold" => &[
            "Segoe UI",
            "Helvetica Neue",
            "Arial",
            "Liberation Sans",
            "Noto Sans",
        ],
        "georgia" => &["Georgia", "Gelasio", "Liberation Serif"],
        "verdana" => &["Verdana", "DejaVu Sans"],
        "century gothic" => &["Century Gothic", "URW Gothic", "Avenir"],
        _ => &[],
    }
}

fn generic(family: &str) -> Family<'static> {
    let lower = family.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if any(&["mono", "courier", "consolas", "code"]) {
        Family::Monospace
    } else if any(&[
        "times",
        "cambria",
        "georgia",
        "garamond",
        "serif",
        "book",
        "palatino",
        "baskerville",
        "minion",
        "caslon",
    ]) && !lower.contains("sans")
    {
        Family::Serif
    } else {
        Family::SansSerif
    }
}

thread_local! {
    static FACES: RefCell<HashMap<(String, bool, bool), Option<Rc<Face>>>> = RefCell::new(HashMap::new());
    /// Advances per em, by face and character.
    static ADVANCES: RefCell<HashMap<(fontdb::ID, char), f32>> = RefCell::new(HashMap::new());
}

fn fonts() -> &'static fontdb::Database {
    &crate::mermaid::FONTS
}

fn face(style: &Style) -> Option<Rc<Face>> {
    let key = (style.family.clone(), style.bold, style.italic);
    if let Some(found) = FACES.with(|f| f.borrow().get(&key).cloned()) {
        return found;
    }
    let mut families: Vec<Family> = vec![Family::Name(&style.family)];
    families.extend(stand_ins(&style.family).iter().map(|f| Family::Name(f)));
    families.push(generic(&style.family));
    families.push(Family::SansSerif);
    let query = Query {
        families: &families,
        weight: if style.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        },
        stretch: Stretch::Normal,
        style: if style.italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
    };
    let found = fonts().query(&query).and_then(|id| {
        let info = fonts().face(id)?;
        let family = info.families.first()?.0.clone();
        let (ascent, descent, leading) = fonts()
            .with_face_data(id, |data, index| {
                let font = FontRef::from_index(data, index).ok()?;
                let m = Metrics::new(&font, Size::new(1.0), LocationRef::default());
                Some((m.ascent, m.descent.abs(), m.leading))
            })
            .flatten()
            .unwrap_or((0.9, 0.25, 0.0));
        Some(Rc::new(Face {
            id,
            squeeze: squeeze(&style.family, &family),
            family,
            ascent,
            descent,
            leading,
        }))
    });
    FACES.with(|f| f.borrow_mut().insert(key, found.clone()));
    found
}

/// What a character takes up when its font lacks it: resvg draws it in
/// another font, about this wide.
fn missing_advance(c: char) -> f32 {
    match unicode_width::UnicodeWidthChar::width(c) {
        Some(2) => 1.0,
        Some(0) => 0.0,
        _ => 0.55,
    }
}

/// The width of `text` in points.
pub fn measure(text: &str, style: &Style) -> f32 {
    let Some(face) = face(style) else {
        return text.chars().map(missing_advance).sum::<f32>() * style.size;
    };
    let text: String = if style.caps {
        text.to_uppercase()
    } else {
        text.to_owned()
    };
    let missing: Vec<char> = ADVANCES.with(|a| {
        let a = a.borrow();
        let mut chars: Vec<char> = text
            .chars()
            .filter(|c| !a.contains_key(&(face.id, *c)))
            .collect();
        chars.sort_unstable();
        chars.dedup();
        chars
    });
    if !missing.is_empty() {
        let found: Vec<(char, f32)> = fonts()
            .with_face_data(face.id, |data, index| {
                let Ok(font) = FontRef::from_index(data, index) else {
                    return Vec::new();
                };
                let charmap = font.charmap();
                let metrics = GlyphMetrics::new(&font, Size::new(1.0), LocationRef::default());
                missing
                    .iter()
                    .map(|&c| {
                        let advance = charmap
                            .map(c)
                            .filter(|g| g.to_u32() != 0)
                            .and_then(|g| metrics.advance_width(g))
                            .unwrap_or_else(|| missing_advance(c));
                        (c, advance)
                    })
                    .collect()
            })
            .unwrap_or_default();
        ADVANCES.with(|a| {
            let mut a = a.borrow_mut();
            for &c in &missing {
                let advance = found
                    .iter()
                    .find(|(f, _)| *f == c)
                    .map(|(_, w)| *w)
                    .unwrap_or_else(|| missing_advance(c));
                a.insert((face.id, c), advance);
            }
        });
    }
    let em: f32 = ADVANCES.with(|a| {
        let a = a.borrow();
        text.chars()
            .map(|c| {
                if c == '\t' {
                    return 2.0;
                }
                a.get(&(face.id, c)).copied().unwrap_or(0.55)
            })
            .sum()
    });
    let size = if style.baseline != 0.0 {
        style.size * 0.65
    } else {
        style.size
    };
    em * size * face.squeeze
}

/// How far above and below the baseline a style's text reaches, and the
/// extra gap its font puts between lines, in points.
fn extent(style: &Style) -> (f32, f32, f32) {
    match face(style) {
        Some(f) => (
            f.ascent * style.size,
            f.descent * style.size,
            f.leading * style.size,
        ),
        None => (style.size * 0.9, style.size * 0.25, 0.0),
    }
}

/// A run of text in one style.
#[derive(Debug, Clone)]
pub struct Run {
    pub text: String,
    pub style: Rc<Style>,
}

/// A piece of a laid-out line.
#[derive(Debug, Clone)]
pub struct Piece {
    /// From the line's start, in points.
    pub x: f32,
    pub width: f32,
    pub text: String,
    pub style: Rc<Style>,
}

#[derive(Debug, Clone, Default)]
pub struct Line {
    pub pieces: Vec<Piece>,
    pub width: f32,
    /// Above and below the baseline, in points.
    pub ascent: f32,
    pub descent: f32,
    pub leading: f32,
}

impl Line {
    /// The line's height at single spacing.
    pub fn height(&self) -> f32 {
        self.ascent + self.descent + self.leading
    }
}

/// Text broken into words and the spaces between them.
fn tokens(runs: &[Run]) -> Vec<(String, Rc<Style>, bool)> {
    let mut out = Vec::new();
    for run in runs {
        let mut word = String::new();
        let mut spacing = false;
        for c in run.text.chars() {
            let is_space = c == ' ' || c == '\t';
            if c == '\n' {
                if !word.is_empty() {
                    out.push((std::mem::take(&mut word), run.style.clone(), spacing));
                }
                out.push(("\n".into(), run.style.clone(), false));
                spacing = false;
                continue;
            }
            if is_space != spacing && !word.is_empty() {
                out.push((std::mem::take(&mut word), run.style.clone(), spacing));
            }
            spacing = is_space;
            word.push(c);
        }
        if !word.is_empty() {
            out.push((word, run.style.clone(), spacing));
        }
    }
    out
}

/// Lines of `runs` no wider than `width` points (`first` for the first
/// line). An empty paragraph still has a line, as tall as `empty`.
pub fn wrap(runs: &[Run], first: f32, width: f32, empty: &Style) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line = Line::default();
    let limit = |lines: &Vec<Line>| if lines.is_empty() { first } else { width };
    let push_piece = |line: &mut Line, text: &str, style: &Rc<Style>, w: f32| {
        match line.pieces.last_mut() {
            Some(last) if Rc::ptr_eq(&last.style, style) || *last.style == **style => {
                last.text.push_str(text);
                last.width += w;
            }
            _ => line.pieces.push(Piece {
                x: line.width,
                width: w,
                text: text.to_owned(),
                style: style.clone(),
            }),
        }
        line.width += w;
        let (a, d, l) = extent(style);
        line.ascent = line.ascent.max(a);
        line.descent = line.descent.max(d);
        line.leading = line.leading.max(l);
    };
    let finish = |line: &mut Line, lines: &mut Vec<Line>| {
        // Spaces at a line's end take no room.
        while let Some(last) = line.pieces.last_mut() {
            let trimmed = last.text.trim_end_matches([' ', '\t']).len();
            if trimmed == last.text.len() {
                break;
            }
            let cut = last.text[trimmed..].to_owned();
            last.text.truncate(trimmed);
            let w = measure(&cut, &last.style);
            last.width -= w;
            line.width -= w;
            if last.text.is_empty() {
                line.pieces.pop();
            } else {
                break;
            }
        }
        if line.ascent == 0.0 {
            let (a, d, l) = extent(empty);
            line.ascent = a;
            line.descent = d;
            line.leading = l;
        }
        lines.push(std::mem::take(line));
    };
    for (text, style, spacing) in tokens(runs) {
        if text == "\n" {
            finish(&mut line, &mut lines);
            continue;
        }
        let w = measure(&text, &style);
        if spacing {
            if !line.pieces.is_empty() {
                push_piece(&mut line, &text, &style, w);
            }
            continue;
        }
        let room = limit(&lines);
        if line.width + w > room + 0.01 && !line.pieces.is_empty() {
            finish(&mut line, &mut lines);
        }
        let room = limit(&lines);
        if w <= room || room <= 0.0 {
            push_piece(&mut line, &text, &style, w);
            continue;
        }
        // A word wider than the line breaks between letters.
        let mut part = String::new();
        let mut part_w = 0.0;
        for c in text.chars() {
            let cw = measure(&c.to_string(), &style);
            if part_w + cw > limit(&lines) && !part.is_empty() {
                push_piece(&mut line, &part, &style, part_w);
                finish(&mut line, &mut lines);
                part.clear();
                part_w = 0.0;
            }
            part.push(c);
            part_w += cw;
        }
        if !part.is_empty() {
            push_piece(&mut line, &part, &style, part_w);
        }
    }
    if !line.pieces.is_empty() || lines.is_empty() {
        finish(&mut line, &mut lines);
    }
    lines
}

/// SVG for a line whose start is at `x` and baseline at `y`, in points.
pub fn svg_line(out: &mut String, line: &Line, x: f32, y: f32) {
    for piece in &line.pieces {
        if piece.text.trim().is_empty() {
            continue;
        }
        let style = &piece.style;
        let found = face(style);
        let family = found
            .as_ref()
            .map(|f| f.family.clone())
            .unwrap_or_else(|| style.family.clone());
        let fitted = match found {
            Some(f) if f.squeeze != 1.0 && piece.text.chars().count() > 1 => format!(
                r#" textLength="{:.2}" lengthAdjust="spacingAndGlyphs""#,
                piece.width - measure(&piece.text[piece.text.trim_end().len()..], style)
            ),
            _ => String::new(),
        };
        let (size, shift) = if style.baseline != 0.0 {
            (style.size * 0.65, -style.baseline * style.size)
        } else {
            (style.size, 0.0)
        };
        let mut decoration = Vec::new();
        if style.underline {
            decoration.push("underline");
        }
        if style.strike {
            decoration.push("line-through");
        }
        let text = if style.caps {
            piece.text.to_uppercase()
        } else {
            piece.text.clone()
        };
        out.push_str(&format!(
            r#"<text x="{:.2}" y="{:.2}" font-family="'{}', {}" font-size="{:.2}"{}{}{fitted} fill="{}"{}{} xml:space="preserve">{}</text>"#,
            x + piece.x,
            y + shift,
            xml_escape(&family).replace('\'', ""),
            match generic(&style.family) {
                Family::Serif => "serif",
                Family::Monospace => "monospace",
                _ => "sans-serif",
            },
            size,
            if style.bold { r#" font-weight="bold""# } else { "" },
            if style.italic { r#" font-style="italic""# } else { "" },
            style.color.css(),
            if style.color.alpha < 1.0 {
                format!(r#" fill-opacity="{:.3}""#, style.color.alpha)
            } else {
                String::new()
            },
            if decoration.is_empty() {
                String::new()
            } else {
                format!(r#" text-decoration="{}""#, decoration.join(" "))
            },
            xml_escape(&text),
        ));
    }
}

/// Draws an SVG `width` pixels wide.
pub fn rasterize(svg: &str, width: u32) -> Option<RgbaImage> {
    let options = usvg::Options {
        fontdb: crate::mermaid::FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).ok()?;
    let size = tree.size();
    let scale = width.max(1) as f32 / size.width().max(1.0);
    let height = ((size.height() * scale).round() as u32).max(1);
    let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height)?;
    pixmap.fill(tiny_skia::Color::WHITE);
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let pixels = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), 255]
    });
    RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Run {
        Run {
            text: text.into(),
            style: Rc::new(Style::default()),
        }
    }

    #[test]
    fn longer_text_measures_wider() {
        let style = Style::default();
        let short = measure("iii", &style);
        let long = measure("WWWWWW", &style);
        assert!(short > 0.0);
        assert!(long > short * 2.0);
        let big = Style {
            size: 22.0,
            ..Style::default()
        };
        assert!((measure("WWWWWW", &big) - long * 2.0).abs() < 0.5);
    }

    #[test]
    fn wraps_at_spaces_and_keeps_every_word() {
        let lines = wrap(
            &[run("the quick brown fox jumps over the lazy dog")],
            80.0,
            80.0,
            &Style::default(),
        );
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.width <= 80.5));
        let words: Vec<String> = lines
            .iter()
            .flat_map(|l| l.pieces.iter().map(|p| p.text.clone()))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            words.join(" "),
            "the quick brown fox jumps over the lazy dog"
        );
    }

    #[test]
    fn a_long_word_breaks_between_letters_and_newlines_break_lines() {
        let lines = wrap(
            &[run("Supercalifragilistic")],
            30.0,
            30.0,
            &Style::default(),
        );
        assert!(lines.len() > 1);
        let lines = wrap(&[run("one\ntwo")], 500.0, 500.0, &Style::default());
        assert_eq!(lines.len(), 2);
        let lines = wrap(&[], 500.0, 500.0, &Style::default());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].height() > 5.0);
    }

    #[test]
    fn draws_an_svg_at_the_width_asked() {
        let mut svg = String::from(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50" viewBox="0 0 100 50">"#,
        );
        let lines = wrap(&[run("Hi")], 100.0, 100.0, &Style::default());
        svg_line(&mut svg, &lines[0], 10.0, 20.0);
        svg.push_str("</svg>");
        let image = rasterize(&svg, 200).unwrap();
        assert_eq!(image.dimensions(), (200, 100));
        assert!(image.pixels().any(|p| p.0[0] < 128));
    }
}
