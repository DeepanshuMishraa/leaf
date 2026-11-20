//! The board drawn as SVG: hand-drawn strokes, fills, pictures and text,
//! for terminals that show pictures and for exporting a board.

use std::collections::HashMap;
use std::fmt::Write as _;

use image::RgbaImage;
use resvg::{tiny_skia, usvg};

use crate::board::{Board, Dash, Element, Fill, FillStyle, Frame, LEADING, Pt, Shape};
use crate::mono::Rgb;

/// The colors the board is drawn in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paint {
    /// The terminal's text color: what Excalidraw's black ink becomes.
    pub ink: Rgb,
    pub paper: Rgb,
    /// Selection, handles and the caret.
    pub accent: Rgb,
}

impl Paint {
    /// Excalidraw's own colors on white, for export.
    pub const LIGHT: Paint = Paint {
        ink: [0x1e, 0x1e, 0x1e],
        paper: [0xff, 0xff, 0xff],
        accent: [0x69, 0x65, 0xdb],
    };

    pub fn is_dark(self) -> bool {
        !crate::theme::is_light(self.paper)
    }
}

/// A color as written in the file.
pub fn parse_color(css: &str) -> Option<Rgb> {
    let digits = css.trim().strip_prefix('#')?;
    let channel = |at: usize, len: usize| {
        let value = u8::from_str_radix(digits.get(at..at + len)?, 16).ok()?;
        Some(if len == 1 { value * 17 } else { value })
    };
    match digits.len() {
        6 | 8 => Some([channel(0, 2)?, channel(2, 2)?, channel(4, 2)?]),
        3 | 4 => Some([channel(0, 1)?, channel(1, 1)?, channel(2, 1)?]),
        _ => None,
    }
}

fn luminance([r, g, b]: Rgb) -> f64 {
    (0.2126 * f64::from(r) + 0.7152 * f64::from(g) + 0.0722 * f64::from(b)) / 255.0
}

/// Whether a color is near enough black to be drawn in the terminal's own
/// text color.
pub fn is_ink(css: &str) -> bool {
    parse_color(css).is_some_and(|rgb| luminance(rgb) < 0.2)
}

/// A board color as shown on `paint`'s paper. Near-black ink takes the
/// terminal's text color; on a dark paper every other color has its
/// lightness turned over, keeping its hue, the way Excalidraw's dark mode
/// does. `None` for "transparent".
pub fn shade(css: &str, paint: Paint) -> Option<Rgb> {
    let rgb = parse_color(css)?;
    if is_ink(css) {
        return Some(paint.ink);
    }
    if !paint.is_dark() {
        return Some(rgb);
    }
    let [r, g, b] = rgb.map(|c| f64::from(c) / 255.0);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let lightness = (max + min) / 2.0;
    let flipped = 0.07 + 0.93 * (1.0 - lightness);
    let scale = |c: f64| {
        // Keep the hue and saturation by moving every channel around the new lightness.
        let spread = if lightness == 0.0 || lightness == 1.0 {
            0.0
        } else {
            (c - lightness) * flipped.min(1.0 - flipped) / lightness.min(1.0 - lightness)
        };
        ((flipped + spread).clamp(0.0, 1.0) * 255.0).round() as u8
    };
    Some([scale(r), scale(g), scale(b)])
}

fn hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Fonts by Excalidraw's font number, with fallbacks for what's installed.
fn family(font: i64) -> &'static str {
    match font {
        2 | 6 => "Helvetica, Arial, Liberation Sans, sans-serif",
        3 | 8 => "Menlo, Cascadia Code, DejaVu Sans Mono, monospace",
        _ => "Chalkboard SE, Comic Sans MS, Comic Neue, Bradley Hand, cursive, sans-serif",
    }
}

/// A small, steady random source, so a shape wobbles the same way each time.
struct Wobble(u64);

impl Wobble {
    fn new(seed: u64) -> Self {
        Wobble(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// A number from -1 to 1.
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 20_001) as f64 / 10_000.0 - 1.0
    }
}

fn point(out: &mut String, command: char, p: Pt) {
    let _ = write!(out, "{command}{:.2} {:.2} ", p.x, p.y);
}

/// Path data for `points`, bowed a little at random per segment when
/// `roughness` is above 0, as a pen held by hand would.
fn rough_path(points: &[Pt], roughness: f64, wobble: &mut Wobble) -> String {
    let mut out = String::new();
    let Some(&first) = points.first() else {
        return out;
    };
    if roughness <= 0.0 {
        return smooth_path_or_straight(points, false);
    }
    let nudge = |w: &mut Wobble, amount: f64| Pt::new(w.next() * amount, w.next() * amount);
    point(&mut out, 'M', first.plus(nudge(wobble, roughness * 0.6)));
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let length = a.distance(b);
        let bow = roughness * (0.6 + length * 0.012).min(3.0);
        let middle = Pt::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let (nx, ny) = if length > 0.0 {
            ((a.y - b.y) / length, (b.x - a.x) / length)
        } else {
            (0.0, 0.0)
        };
        let off = wobble.next() * bow;
        let control = Pt::new(middle.x + nx * off, middle.y + ny * off);
        point(&mut out, 'Q', control);
        let end = b.plus(nudge(wobble, roughness * 0.6));
        let _ = write!(out, "{:.2} {:.2} ", end.x, end.y);
    }
    out
}

/// Path data through `points`, rounded between them.
fn smooth_path(points: &[Pt]) -> String {
    let mut out = String::new();
    let Some(&first) = points.first() else {
        return out;
    };
    point(&mut out, 'M', first);
    for pair in points.windows(2).skip(1) {
        let (a, b) = (pair[0], pair[1]);
        point(&mut out, 'Q', a);
        let _ = write!(out, "{:.2} {:.2} ", (a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
    }
    if let Some(&last) = points.last() {
        point(&mut out, 'L', last);
    }
    out
}

/// A rounded rectangle's outline after turning, as Excalidraw rounds them.
fn rounded(element: &Element) -> Vec<Pt> {
    let f = element.frame();
    let r = (f.width().min(f.height()) * 0.25).min(32.0);
    let corners = [
        (Pt::new(f.x1 - r, f.y0 + r), -std::f64::consts::FRAC_PI_2),
        (Pt::new(f.x1 - r, f.y1 - r), 0.0),
        (Pt::new(f.x0 + r, f.y1 - r), std::f64::consts::FRAC_PI_2),
        (Pt::new(f.x0 + r, f.y0 + r), std::f64::consts::PI),
    ];
    let c = f.center();
    let mut points: Vec<Pt> = corners
        .iter()
        .flat_map(|(center, start)| {
            (0..=6).map(move |i| {
                let a = start + f64::from(i) / 6.0 * std::f64::consts::FRAC_PI_2;
                Pt::new(center.x + r * a.cos(), center.y + r * a.sin())
            })
        })
        .map(|p| p.turn(c, element.angle))
        .collect();
    if let Some(&first) = points.first() {
        points.push(first);
    }
    points
}

/// Patterns for hatched fills, one per color and style.
#[derive(Default)]
struct Patterns {
    defs: String,
    made: HashMap<(String, FillStyle), String>,
}

impl Patterns {
    /// What a fill attribute names: a color, or a hatch pattern in it.
    fn fill(&mut self, fill: &Fill, paint: Paint, stroke: f64) -> Option<String> {
        let color = hex(shade(&fill.color, paint)?);
        if fill.style == FillStyle::Solid {
            return Some(color);
        }
        let key = (color.clone(), fill.style);
        if let Some(id) = self.made.get(&key) {
            return Some(format!("url(#{id})"));
        }
        let id = format!("hatch{}", self.made.len());
        let gap = (stroke * 4.0).max(6.0);
        let mut lines = format!(
            r#"<line x1="0" y1="0" x2="0" y2="{gap}" stroke="{color}" stroke-width="{:.2}"/>"#,
            stroke / 2.0
        );
        if fill.style == FillStyle::CrossHatch {
            let _ = write!(
                lines,
                r#"<line x1="0" y1="0" x2="{gap}" y2="0" stroke="{color}" stroke-width="{:.2}"/>"#,
                stroke / 2.0
            );
        }
        let _ = write!(
            self.defs,
            r#"<pattern id="{id}" patternUnits="userSpaceOnUse" width="{gap}" height="{gap}" patternTransform="rotate(-41)">{lines}</pattern>"#
        );
        self.made.insert(key, id.clone());
        Some(format!("url(#{id})"))
    }
}

/// What's drawn over the board while it's being worked on.
#[derive(Default)]
pub struct Overlay<'a> {
    /// Text being written over, left out of the board.
    pub hidden: Option<&'a str>,
    /// An element being drawn.
    pub preview: Option<&'a Element>,
    /// Outlines of what's selected, corners clockwise.
    pub selection: Vec<[Pt; 4]>,
    /// Squares to drag to resize, and the circle to drag to turn.
    pub handles: Vec<Pt>,
    pub turn: Option<Pt>,
    pub marquee: Option<Frame>,
    /// Text being written: where, what, its size and color.
    pub caret: Option<(Pt, &'a str, f64, &'a str)>,
}

fn element(out: &mut String, board: &Board, e: &Element, paint: Paint, patterns: &mut Patterns) {
    let Some(color) = shade(&e.color, paint).map(hex) else {
        return;
    };
    let mut wobble = Wobble::new(e.seed());
    let roughness = e.roughness().clamp(0.0, 3.0);
    let width = e.stroke.max(0.5);
    let dash = match e.dash {
        Dash::Solid => String::new(),
        Dash::Dashed => format!(r#" stroke-dasharray="8 {:.1}""#, 8.0 + width),
        Dash::Dotted => format!(r#" stroke-dasharray="1.5 {:.1}""#, 6.0 + width),
    };
    let degrees = e.angle.to_degrees();
    let c = e.center();
    let turn = if e.angle == 0.0 {
        String::new()
    } else {
        format!(r#" transform="rotate({degrees:.3} {:.2} {:.2})""#, c.x, c.y)
    };
    let _ = write!(out, r#"<g opacity="{:.2}">"#, e.opacity());
    let stroke_attrs = format!(
        r#"fill="none" stroke="{color}" stroke-width="{width:.2}" stroke-linecap="round" stroke-linejoin="round"{dash}"#
    );
    // Hand-drawn lines go over twice, slightly apart; dashes go once so they line up.
    let passes = if roughness > 0.0 && e.dash == Dash::Solid {
        2
    } else {
        1
    };
    let f = e.frame();
    match &e.shape {
        Shape::Text { text, size } => {
            let anchor = match e.align() {
                "center" => ("middle", (f.x0 + f.x1) / 2.0),
                "right" => ("end", f.x1),
                _ => ("start", f.x0),
            };
            text_lines(
                out,
                text,
                Pt::new(anchor.1, f.y0),
                *size,
                &color,
                e.font(),
                anchor.0,
                &turn,
            );
        }
        Shape::Image { file, .. } => match board.picture(file) {
            Some(url) => {
                let _ = write!(
                    out,
                    r#"<image x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" preserveAspectRatio="none" xlink:href="{}"{turn}/>"#,
                    f.x0,
                    f.y0,
                    f.width(),
                    f.height(),
                    escape(url)
                );
            }
            None => labelled_box(out, e, &color, "picture missing"),
        },
        Shape::Frame { name, .. } => {
            let _ = write!(
                out,
                r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" rx="8" fill="none" stroke="{color}" stroke-opacity="0.5" stroke-width="1"{turn}/>"#,
                f.x0,
                f.y0,
                f.width(),
                f.height()
            );
            let name = if name.is_empty() { "Frame" } else { name };
            let _ = write!(
                out,
                r#"<text x="{:.2}" y="{:.2}" font-size="14" font-family="{}" fill="{color}" fill-opacity="0.6">{}</text>"#,
                f.x0,
                f.y0 - 6.0,
                family(2),
                escape(name)
            );
        }
        Shape::Other { kind, .. } => labelled_box(out, e, &color, kind),
        Shape::Draw { .. } => {
            for (points, _) in e.paths() {
                let _ = write!(
                    out,
                    r#"<path d="{}" {stroke_attrs} stroke-width="{:.2}"/>"#,
                    smooth_path(&points),
                    width * 1.25
                );
            }
        }
        _ => {
            let smooth = matches!(e.shape, Shape::Ellipse { .. })
                || (e.rounded() && matches!(e.shape, Shape::Rectangle { .. }));
            let outline: Vec<(Vec<Pt>, bool)> =
                if e.rounded() && matches!(e.shape, Shape::Rectangle { .. }) {
                    vec![(rounded(e), true)]
                } else {
                    e.paths()
                };
            if let Some(fill) = &e.fill
                && let Some(paint) = patterns.fill(fill, paint, width)
            {
                for (points, closed) in &outline {
                    if *closed {
                        let _ = write!(
                            out,
                            r#"<path d="{}Z" fill="{paint}" stroke="none"/>"#,
                            smooth_path_or_straight(points, smooth)
                        );
                    }
                }
            }
            for (points, _) in &outline {
                for _ in 0..passes {
                    let d = if smooth {
                        let shift = Pt::new(wobble.next() * roughness, wobble.next() * roughness);
                        let moved: Vec<Pt> = points.iter().map(|p| p.plus(shift)).collect();
                        smooth_path(&moved)
                    } else {
                        rough_path(points, roughness, &mut wobble)
                    };
                    let _ = write!(out, r#"<path d="{d}" {stroke_attrs}/>"#);
                }
            }
            for tip in e.tips() {
                let (paths, solid) = tip.paths();
                for points in paths {
                    let fill = if solid { color.as_str() } else { "none" };
                    let _ = write!(
                        out,
                        r#"<path d="{}" fill="{fill}" stroke="{color}" stroke-width="{width:.2}" stroke-linecap="round" stroke-linejoin="round"/>"#,
                        rough_path(&points, roughness * 0.5, &mut wobble)
                    );
                }
            }
        }
    }
    out.push_str("</g>");
}

fn smooth_path_or_straight(points: &[Pt], smooth: bool) -> String {
    if smooth {
        return smooth_path(points);
    }
    let mut out = String::new();
    for (i, p) in points.iter().enumerate() {
        point(&mut out, if i == 0 { 'M' } else { 'L' }, *p);
    }
    out
}

fn labelled_box(out: &mut String, e: &Element, color: &str, label: &str) {
    let f = e.frame();
    let _ = write!(
        out,
        r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="none" stroke="{color}" stroke-width="1" stroke-dasharray="6 6"/><text x="{:.2}" y="{:.2}" font-size="14" font-family="{}" fill="{color}">[{}]</text>"#,
        f.x0,
        f.y0,
        f.width(),
        f.height(),
        f.x0 + 8.0,
        f.y0 + 20.0,
        family(2),
        escape(label)
    );
}

#[allow(clippy::too_many_arguments)]
fn text_lines(
    out: &mut String,
    text: &str,
    at: Pt,
    size: f64,
    color: &str,
    font: i64,
    anchor: &str,
    turn: &str,
) {
    let _ = write!(
        out,
        r#"<text font-size="{size:.2}" font-family="{}" fill="{color}" text-anchor="{anchor}"{turn}>"#,
        family(font)
    );
    for (i, line) in text.split('\n').enumerate() {
        let y = at.y + size * (i as f64 * LEADING + 0.98);
        let _ = write!(
            out,
            r#"<tspan x="{:.2}" y="{y:.2}" xml:space="preserve">{}</tspan>"#,
            at.x,
            escape(line)
        );
    }
    out.push_str("</text>");
}

/// The part of the board inside `window`, `width` × `height` pixels.
/// `scale` is pixels per board pixel, for drawing the overlay the same size
/// at every zoom.
pub fn svg(
    board: &Board,
    window: Frame,
    (width, height): (u32, u32),
    paint: Paint,
    overlay: &Overlay,
) -> String {
    let scale = f64::from(width) / window.width().max(1.0);
    let mut body = String::new();
    let mut patterns = Patterns::default();
    let seen = window.grow(64.0);
    for e in &board.elements {
        let b = e.bounds();
        let off = b.x1 < seen.x0 || b.x0 > seen.x1 || b.y1 < seen.y0 || b.y0 > seen.y1;
        if off || Some(e.id.as_str()) == overlay.hidden {
            continue;
        }
        element(&mut body, board, e, paint, &mut patterns);
    }
    if let Some(e) = overlay.preview {
        element(&mut body, board, e, paint, &mut patterns);
    }
    let accent = hex(paint.accent);
    let thin = 1.5 / scale;
    for corners in &overlay.selection {
        let mut d = String::new();
        for (i, p) in corners.iter().enumerate() {
            point(&mut d, if i == 0 { 'M' } else { 'L' }, *p);
        }
        let _ = write!(
            body,
            r#"<path d="{d}Z" fill="none" stroke="{accent}" stroke-width="{thin:.3}" stroke-dasharray="{:.2} {:.2}"/>"#,
            4.0 / scale,
            3.0 / scale
        );
    }
    let size = 8.0 / scale;
    let paper = hex(paint.paper);
    for p in &overlay.handles {
        let _ = write!(
            body,
            r#"<rect x="{:.2}" y="{:.2}" width="{size:.2}" height="{size:.2}" rx="{:.2}" fill="{paper}" stroke="{accent}" stroke-width="{thin:.3}"/>"#,
            p.x - size / 2.0,
            p.y - size / 2.0,
            size / 4.0
        );
    }
    if let Some(p) = overlay.turn {
        let _ = write!(
            body,
            r#"<circle cx="{:.2}" cy="{:.2}" r="{:.2}" fill="{paper}" stroke="{accent}" stroke-width="{thin:.3}"/>"#,
            p.x,
            p.y,
            size / 1.6
        );
    }
    if let Some(m) = overlay.marquee {
        let _ = write!(
            body,
            r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{accent}" fill-opacity="0.08" stroke="{accent}" stroke-width="{thin:.3}"/>"#,
            m.x0,
            m.y0,
            m.width(),
            m.height()
        );
    }
    if let Some((at, text, size, color)) = overlay.caret {
        let color = shade(color, paint).map_or_else(|| hex(paint.ink), hex);
        text_lines(&mut body, text, at, size, &color, 1, "start", "");
        let lines: Vec<&str> = text.split('\n').collect();
        let row = lines.len().saturating_sub(1) as f64;
        // The caret goes after the last line, placed by the same measure text is.
        let x = at.x
            + lines.last().map_or(0, |l| l.chars().count()) as f64 * size * crate::board::ADVANCE;
        let y = at.y + row * size * LEADING;
        let _ = write!(
            body,
            r#"<line x1="{x:.2}" y1="{:.2}" x2="{x:.2}" y2="{:.2}" stroke="{accent}" stroke-width="{:.3}"/>"#,
            y + size * 0.1,
            y + size * 1.15,
            2.0 / scale
        );
    }
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{width}" height="{height}" viewBox="{:.3} {:.3} {:.3} {:.3}"><defs>{}</defs><rect x="{:.3}" y="{:.3}" width="{:.3}" height="{:.3}" fill="{paper}"/>{body}</svg>"#,
        window.x0,
        window.y0,
        window.width(),
        window.height(),
        patterns.defs,
        window.x0,
        window.y0,
        window.width(),
        window.height(),
    )
}

/// The whole board in Excalidraw's own colors on white, for saving as a
/// picture. `None` for an empty board.
pub fn export(board: &Board, scale: f64) -> Option<String> {
    let window = board.bounds()?.grow(20.0);
    let size = (
        (window.width() * scale).ceil().max(1.0) as u32,
        (window.height() * scale).ceil().max(1.0) as u32,
    );
    Some(svg(board, window, size, Paint::LIGHT, &Overlay::default()))
}

/// Paints an SVG made here.
pub fn rasterize(svg: &str) -> Option<RgbaImage> {
    let options = usvg::Options {
        fontdb: crate::mermaid::FONTS.clone(),
        ..Default::default()
    };
    let tree = usvg::Tree::from_str(svg, &options).ok()?;
    let size = tree.size().to_int_size();
    let mut pixmap = tiny_skia::Pixmap::new(size.width(), size.height())?;
    resvg::render(
        &tree,
        tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let pixels = pixmap.pixels().iter().flat_map(|p| {
        let c = p.demultiply();
        [c.red(), c.green(), c.blue(), c.alpha()]
    });
    RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::INK;

    const DARK: Paint = Paint {
        ink: [230, 230, 230],
        paper: [20, 20, 24],
        accent: [80, 160, 255],
    };

    #[test]
    fn black_ink_takes_the_terminal_text_color_and_dark_paper_flips_the_rest() {
        assert_eq!(shade(INK, DARK), Some(DARK.ink));
        assert_eq!(shade("transparent", DARK), None);
        assert_eq!(shade("#e03131", Paint::LIGHT), Some([0xe0, 0x31, 0x31]));
        // A pale fill turns dark on dark paper, still reddish.
        let [r, g, b] = shade("#ffc9c9", DARK).expect("parses");
        assert!(luminance([r, g, b]) < 0.4, "{r} {g} {b}");
        assert!(r > g && r > b);
    }

    #[test]
    fn a_board_paints_its_strokes_fills_and_text() {
        let mut board = Board::default();
        let mut boxed = Element::new(
            Shape::Rectangle { w: 80.0, h: 40.0 },
            Pt::new(10.0, 10.0),
            "#e03131",
        );
        boxed.fill = Some(Fill {
            color: "#e03131".into(),
            style: FillStyle::Solid,
        });
        board.elements.push(boxed);
        board.elements.push(Element::new(
            Shape::Text {
                text: "a < b".into(),
                size: 20.0,
            },
            Pt::new(10.0, 70.0),
            INK,
        ));
        let window = Frame {
            x0: 0.0,
            y0: 0.0,
            x1: 100.0,
            y1: 100.0,
        };
        let svg = svg(
            &board,
            window,
            (100, 100),
            Paint::LIGHT,
            &Overlay::default(),
        );
        assert!(svg.contains("a &lt; b"));
        let pixels = rasterize(&svg).expect("renders");
        assert_eq!(pixels.dimensions(), (100, 100));
        let inside = pixels.get_pixel(50, 30);
        assert_eq!(&inside.0[..3], &[0xe0, 0x31, 0x31], "solid fill");
        let paper = pixels.get_pixel(97, 3);
        assert_eq!(&paper.0[..3], &[255, 255, 255]);
    }

    #[test]
    fn the_same_seed_wobbles_the_same_way() {
        let points = [Pt::new(0.0, 0.0), Pt::new(100.0, 0.0)];
        let a = rough_path(&points, 1.0, &mut Wobble::new(7));
        let b = rough_path(&points, 1.0, &mut Wobble::new(7));
        let c = rough_path(&points, 1.0, &mut Wobble::new(8));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            rough_path(&points, 0.0, &mut Wobble::new(7)),
            "M0.00 0.00 L100.00 0.00 "
        );
    }

    #[test]
    fn export_frames_the_whole_board() {
        assert_eq!(export(&Board::default(), 1.0), None);
        let mut board = Board::default();
        board.elements.push(Element::new(
            Shape::Ellipse { w: 50.0, h: 30.0 },
            Pt::new(0.0, 0.0),
            INK,
        ));
        let svg = export(&board, 2.0).expect("has content");
        assert!(svg.contains(r#"width="180" height="140""#), "{svg}");
    }
}
