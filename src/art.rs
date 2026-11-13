//! leaf's dithered surfaces, drawn with dither-kit's ordered dither in the
//! colors of the Rex or terminal theme. Bars and panels are a quiet tint of
//! the background; where a bar meets the page it dissolves into it through
//! one row of half-block pixels. Dither only goes in rows that hold no text.

use dither_ratatui::dither_core::{Field, Matrix, PixelGrid, Rgba, Tone};
use dither_ratatui::raster::{fill_bg, paint, pixel_size};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::mono::Rgb;
use crate::scheme::Colors;
use crate::theme::{self, Theme};

/// Backgrounds assumed when only light or dark is known.
const DARK_PAPER: Rgb = [24, 24, 27];
const LIGHT_PAPER: Rgb = [250, 250, 250];
/// How far a bar's tint moves from the background toward the theme's accent.
const SURFACE_TINT: f32 = 0.1;
/// Where on a tone's ramp (0 near-black, 1 pale) pixels sit. Each mirrors
/// the other around the background: pastels that fade into white on a
/// light theme, deep jewel tones that fade into black on a dark one.
const DARK_LEVEL: f32 = 0.38;
const LIGHT_LEVEL: f32 = 0.5;
/// Degrees the second tone turns from the first when the theme's magenta
/// is a grey or too close to its accent to tell apart.
const ACCENT_TURN: f32 = 50.0;
/// Closest the two tones' hues may be, in degrees.
const MIN_SPREAD: f32 = 25.0;
/// The hue taken when the theme's accent is a grey.
const DEFAULT_HUE: f32 = 250.0;

/// The OKLCH hue of an sRGB color in degrees, or `None` for a grey.
fn hue([r, g, b]: Rgb) -> Option<f32> {
    let linear = |c: u8| {
        let c = f32::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(r), linear(g), linear(b));
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_99 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    let a = 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s;
    let b = 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s;
    let chroma = (a * a + b * b).sqrt();
    (chroma > 0.02).then(|| b.atan2(a).to_degrees().rem_euclid(360.0))
}
/// How far apart two hues are around the circle, in degrees.
fn circular_gap(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(360.0);
    d.min(360.0 - d)
}

/// Widest a rule beside a page number reaches on each side.
const RULE_REACH: u16 = 16;

/// The two colors everything is drawn in, taken from a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ink {
    /// Bars and panels.
    pub surface: Rgba,
    /// Dithered pixels: the theme's accent, in dither-kit's ramp for its hue.
    pub mark: Rgba,
    /// A second tone that gradients dither toward: the theme's magenta.
    pub accent: Rgba,
    background: Rgba,
}

impl Ink {
    /// Bars tinted from the theme's background toward its accent; pixels in
    /// dither-kit's perceptual ramps for the hues of its accent and its
    /// magenta, so each theme dithers in its own colors and a muted one
    /// still gets clear color.
    pub fn of(theme: Theme) -> Self {
        let rgba = |[r, g, b]: Rgb| Rgba::rgb(r, g, b);
        let first = hue(theme.accent)
            .or_else(|| hue(theme.blue))
            .unwrap_or(DEFAULT_HUE);
        let second = hue(theme.magenta)
            .filter(|&other| circular_gap(first, other) >= MIN_SPREAD)
            .unwrap_or(first + ACCENT_TURN);
        let level = if theme.is_light() {
            LIGHT_LEVEL
        } else {
            DARK_LEVEL
        };
        Ink {
            surface: rgba(theme::mix(theme.background, theme.accent, SURFACE_TINT)),
            mark: Tone::Hue(first).shade(level),
            accent: Tone::Hue(second).shade(level - 0.03),
            background: rgba(theme.background),
        }
    }

    /// Grey tints for a background whose palette isn't known.
    pub fn plain(background: Rgb) -> Self {
        let toward = if theme::is_light(background) {
            [0, 0, 0]
        } else {
            [255, 255, 255]
        };
        Ink::of(Theme {
            background,
            red: toward,
            green: toward,
            blue: toward,
            accent: toward,
            magenta: toward,
        })
    }

    /// The theme's own tints when its colors are known, else grey ones for
    /// a light or dark background.
    pub fn of_colors(colors: Colors) -> Self {
        colors.theme.map_or_else(
            || {
                Ink::plain(if colors.light {
                    LIGHT_PAPER
                } else {
                    DARK_PAPER
                })
            },
            Ink::of,
        )
    }

    /// `mark` or `accent` for pixel `(x, y)`, dithered between them by `t` (0..1).
    fn blend(self, x: i32, y: i32, t: f32) -> Rgba {
        if Matrix::Bayer8.on(x + 3, y + 5, t) {
            self.accent
        } else {
            self.mark
        }
    }

    /// `blend`, sunk toward the background as `level` (0..1) drops, so a
    /// fade thins out in brightness as well as in pixels.
    fn lit(self, x: i32, y: i32, t: f32, level: f32) -> Rgba {
        self.blend(x, y, t)
            .mix(self.background, (1.0 - level.clamp(0.0, 1.0)) * FADE_DEPTH)
    }
}

/// How far the faintest pixels of a fade sink toward the background.
const FADE_DEPTH: f32 = 0.65;

/// Whether pixel `(x, y)` of a one-row strip is lit at `density`. A single
/// row of a Bayer matrix holds only a quarter of its levels, so the matrix
/// row steps every four pixels and any sixteen in a row see every level.
fn row_on(x: i32, y: i32, density: f32) -> bool {
    Matrix::Bayer4.on(x, x.div_euclid(4) + y, density)
}

/// The frame a screen sits in: a bar over its first `header` rows that
/// dissolves down through the blank row after them, a bar on its last row
/// that dissolves up through the blank row before it, and curtains either
/// side of the column `x..x + width` in between.
pub fn chrome(buf: &mut Buffer, area: Rect, x: u16, width: u16, header: u16, ink: Ink) {
    if area.height < header + 4 {
        return;
    }
    let row = |y: u16, height: u16| Rect::new(area.x, y, area.width, height);
    surface(buf, row(area.y, header), ink);
    dissolve(buf, row(area.y + header, 1), ink, Bar::Above);
    let footer = area.bottom() - 1;
    surface(buf, row(footer, 1), ink);
    dissolve(buf, row(footer - 1, 1), ink, Bar::Below);
    let top = area.y + header + 1;
    let column = Rect::new(x, top, width, (footer - 1).saturating_sub(top));
    curtains(buf, area, column, ink);
}

/// Columns kept clear between a curtain and the text.
const CURTAIN_GAP: u16 = 2;
/// Space either side of the column narrower than this gets no curtain.
const MIN_CURTAIN: u16 = 4;
/// Densest a curtain gets, at the screen's edge.
const CURTAIN_DENSITY: f32 = 0.62;

/// Fills the space either side of `column` with a two-tone ordered-dither
/// gradient: densest at the screen's edges, gone before the text. `column`'s
/// rows are the ones covered.
pub fn curtains(buf: &mut Buffer, area: Rect, column: Rect, ink: Ink) {
    let left = Rect::new(
        area.x,
        column.y,
        column.x.saturating_sub(area.x + CURTAIN_GAP),
        column.height,
    );
    let right_x = column.right() + CURTAIN_GAP;
    let right = Rect::new(
        right_x,
        column.y,
        area.right().saturating_sub(right_x),
        column.height,
    );
    for (side, outer_left) in [(left, true), (right, false)] {
        let side = side.intersection(buf.area);
        if side.width < MIN_CURTAIN || side.height == 0 {
            continue;
        }
        let (w, h) = pixel_size(side);
        let mut grid = PixelGrid::new(w, h);
        for y in 0..h {
            let v = y as f32 / h.max(2).saturating_sub(1) as f32;
            for x in 0..w {
                let from_edge = if outer_left { x } else { w - 1 - x };
                let near = (1.0 - from_edge as f32 / w as f32).powf(1.8);
                let (gx, gy) = (side.x as i32 + x as i32, side.y as i32 * 2 + y as i32);
                if Matrix::Bayer8.on(gx, gy, CURTAIN_DENSITY * near) {
                    grid.set(x as i32, y as i32, ink.lit(gx, gy, v, near));
                }
            }
        }
        paint(&grid, side, buf, ink.background);
    }
}

/// A one-row two-tone fade filling the rest of a header row after its
/// text: densest where the text ends, gone at the row's end.
pub fn trail(buf: &mut Buffer, row: Rect, ink: Ink) {
    let row = Rect { height: 1, ..row }.intersection(buf.area);
    if row.width < 2 {
        return;
    }
    let (w, h) = pixel_size(row);
    let mut grid = PixelGrid::new(w, h);
    for x in 0..w {
        let t = x as f32 / w as f32;
        let density = 0.6 * (1.0 - t).powf(1.3);
        for y in 0..h {
            let (gx, gy) = (row.x as i32 + x as i32, row.y as i32 * 2 + y as i32);
            if row_on(gx, gy, density) {
                grid.set(x as i32, y as i32, ink.lit(gx, gy, t, 1.0 - t));
            }
        }
    }
    paint(&grid, row, buf, ink.background);
}

/// Rows the fade after the end of a list or diff runs over.
const END_ROWS: u16 = 6;

/// The space after the last line of something shorter than the screen:
/// a two-tone fade over its first few rows, densest at the top.
pub fn end(buf: &mut Buffer, area: Rect, ink: Ink) {
    let area = Rect {
        height: area.height.min(END_ROWS),
        ..area
    }
    .intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let (w, h) = pixel_size(area);
    let mut grid = PixelGrid::new(w, h);
    for y in 0..h {
        let near = (1.0 - y as f32 / h as f32).powf(1.6);
        for x in 0..w {
            let t = x as f32 / w.max(1) as f32;
            // Strongest in the middle, so it reads as a glow, not a band.
            let middle = 1.0 - (2.0 * t - 1.0).powi(2);
            let level = near * middle;
            let (gx, gy) = (area.x as i32 + x as i32, area.y as i32 * 2 + y as i32);
            if Matrix::Bayer8.on(gx, gy, 0.45 * level) {
                grid.set(x as i32, y as i32, ink.lit(gx, gy, t, level));
            }
        }
    }
    paint(&grid, area, buf, ink.background);
}

/// Which side of a dissolving row its bar is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bar {
    Above,
    Below,
}

/// Tints `area` as a bar or panel; text drawn over it keeps the tint.
pub fn surface(buf: &mut Buffer, area: Rect, ink: Ink) {
    fill_bg(area, buf, ink.surface);
}

/// Dissolves the bar on `bar`'s side of the blank one-row `row` into the
/// page: dense next to the bar, sparse away from it.
pub fn dissolve(buf: &mut Buffer, row: Rect, ink: Ink, bar: Bar) {
    let row = Rect { height: 1, ..row }.intersection(buf.area);
    if row.is_empty() {
        return;
    }
    let (w, h) = pixel_size(row);
    let mut grid = PixelGrid::new(w, h);
    for y in 0..h {
        let from_bar = match bar {
            Bar::Above => y,
            Bar::Below => h - 1 - y,
        };
        let density = [0.5, 0.16][from_bar.min(1)];
        for x in 0..w {
            // Screen-wide, so neighbouring rows' patterns line up.
            let (gx, gy) = ((row.x as usize + x) as i32, (row.y as usize * 2 + y) as i32);
            if row_on(gx, gy, density) {
                grid.set(x as i32, y as i32, ink.surface);
            }
        }
    }
    paint(&grid, row, buf, ink.background);
}

/// A checkered shadow one cell below and to the right of `panel`.
pub fn shadow(buf: &mut Buffer, panel: Rect, ink: Ink) {
    let right = Rect::new(panel.right(), panel.y + 1, 1, panel.height);
    let below = Rect::new(panel.x + 1, panel.bottom(), panel.width, 1);
    for side in [right, below] {
        let side = side.intersection(buf.area);
        if side.is_empty() {
            continue;
        }
        let (w, h) = pixel_size(side);
        let mut grid = PixelGrid::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (gx, gy) = (side.x as i32 + x as i32, side.y as i32 * 2 + y as i32);
                if Matrix::Bayer2.on(gx, gy, 0.5) {
                    grid.set(x as i32, y as i32, ink.lit(gx, gy, 0.0, 0.3));
                }
            }
        }
        paint(&grid, side, buf, ink.background);
    }
}

/// Dithered rules fading out either side of a label `label_width` cells
/// wide, centred in the blank-but-for-it one-row `row`.
pub fn rule(buf: &mut Buffer, row: Rect, label_width: u16, ink: Ink) {
    let mid = row.x + row.width / 2;
    let half = label_width.div_ceil(2) + 2;
    let left_end = mid.saturating_sub(half).max(row.x);
    let left_width = (left_end - row.x).min(RULE_REACH);
    let right_x = mid + half;
    let right_width = row.right().saturating_sub(right_x).min(RULE_REACH);
    let sides = [
        (Rect::new(left_end - left_width, row.y, left_width, 1), true),
        (Rect::new(right_x, row.y, right_width, 1), false),
    ];
    for (side, toward_right) in sides {
        let side = side.intersection(buf.area);
        if side.is_empty() {
            continue;
        }
        let (w, h) = pixel_size(side);
        let mut grid = PixelGrid::new(w, h);
        for x in 0..w {
            // Densest beside the label.
            let near = if toward_right { x + 1 } else { w - x };
            let level = 1.0 - near as f32 / (w + 1) as f32;
            for y in 0..h {
                let (gx, gy) = (side.x as i32 + x as i32, side.y as i32 * 2 + y as i32);
                if row_on(gx, gy, 0.75 * level) {
                    grid.set(x as i32, y as i32, ink.lit(gx, gy, 0.0, level));
                }
            }
        }
        paint(&grid, side, buf, ink.background);
    }
}

/// A moving two-tone dithered `field` filling `area`, for something on its
/// way: an image downloading, or a screen loading. `time` is in seconds.
pub fn field(buf: &mut Buffer, area: Rect, ink: Ink, field: Field, time: f32) {
    let area = area.intersection(buf.area);
    let (w, h) = pixel_size(area);
    let mut grid = PixelGrid::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / w.max(1) as f32, y as f32 / h.max(1) as f32);
            // Fading toward the edges, so the field sits in the space as a glow.
            let (dx, dy) = (u - 0.5, v - 0.5);
            let glow = (1.0 - (dx * dx + dy * dy).sqrt() * 1.5).clamp(0.0, 1.0);
            let i = field.sample(u, v, time) * glow;
            let (gx, gy) = (area.x as i32 + x as i32, area.y as i32 * 2 + y as i32);
            if Matrix::Bayer8.on(gx, gy, i * 0.55) {
                grid.set(x as i32, y as i32, ink.lit(gx, gy, v, i));
            }
        }
    }
    paint(&grid, area, buf, ink.background);
}

/// Seconds since the process started, the clock moving fields share.
pub fn clock() -> f32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs_f32()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const INK: Ink = Ink {
        surface: Rgba::rgb(40, 40, 50),
        mark: Rgba::rgb(90, 90, 120),
        accent: Rgba::rgb(120, 90, 120),
        background: Rgba::rgb(20, 20, 20),
    };

    fn lit(buf: &Buffer, area: Rect) -> usize {
        area.positions()
            .filter(|&p| matches!(buf[p].symbol(), "▀" | "▄"))
            .count()
    }

    #[test]
    fn tints_follow_the_theme() {
        let dark = Ink::of(Theme {
            background: [10, 10, 10],
            red: [200, 0, 0],
            green: [0, 200, 0],
            blue: [0, 0, 250],
            accent: [0, 0, 250],
            magenta: [200, 0, 200],
        });
        assert!(
            dark.surface.b > dark.surface.r,
            "leans toward the theme's accent"
        );
        assert!(
            dark.mark.b > dark.surface.b,
            "marks stand out more than bars"
        );
        let light = Ink::plain([250, 250, 250]);
        assert!(light.surface.r < 250, "a light theme's bars go darker");
        let dark = Ink::plain([20, 20, 20]);
        assert!(dark.surface.r > 20, "a dark theme's bars go lighter");
    }

    #[test]
    fn a_dissolve_is_denser_next_to_its_bar() {
        let row = Rect::new(0, 0, 64, 1);
        let count = |bar| {
            let mut buf = Buffer::empty(row);
            dissolve(&mut buf, row, INK, bar);
            let (mut top, mut bottom) = (0, 0);
            for p in row.positions() {
                let cell = &buf[p];
                match cell.symbol() {
                    "▀" if cell.bg == Color::Reset => top += 1,
                    "▀" => {
                        top += 1;
                        bottom += 1
                    }
                    "▄" => bottom += 1,
                    _ => {}
                }
            }
            (top, bottom)
        };
        let (top, bottom) = count(Bar::Above);
        assert!(top > bottom * 2, "{top} {bottom}");
        let (top, bottom) = count(Bar::Below);
        assert!(bottom > top * 2, "{top} {bottom}");
    }

    #[test]
    fn a_shadow_falls_outside_its_panel_and_the_screen_clips_it() {
        let area = Rect::new(0, 0, 40, 20);
        let panel = Rect::new(10, 5, 20, 10);
        let mut buf = Buffer::empty(area);
        shadow(&mut buf, panel, INK);
        assert_eq!(lit(&buf, panel), 0);
        assert!(lit(&buf, Rect::new(30, 6, 1, 10)) > 0);
        assert!(lit(&buf, Rect::new(11, 15, 20, 1)) > 0);
        shadow(&mut buf, Rect::new(30, 15, 10, 5), INK);
    }

    #[test]
    fn a_rule_leaves_its_label_alone() {
        let row = Rect::new(0, 0, 76, 1);
        let mut buf = Buffer::empty(row);
        rule(&mut buf, row, 7, INK);
        assert_eq!(lit(&buf, Rect::new(38 - 6, 0, 12, 1)), 0);
        assert!(lit(&buf, Rect::new(0, 0, 32, 1)) > 0);
        assert!(lit(&buf, Rect::new(44, 0, 32, 1)) > 0);
    }

    #[test]
    fn a_rule_fits_a_narrow_row() {
        let row = Rect::new(0, 0, 5, 1);
        let mut buf = Buffer::empty(row);
        rule(&mut buf, row, 7, INK);
        assert_eq!(lit(&buf, row), 0);
    }

    #[test]
    fn curtains_stay_out_of_the_column_and_its_gap() {
        let area = Rect::new(0, 0, 160, 30);
        let column = Rect::new(42, 0, 76, 27);
        let mut buf = Buffer::empty(area);
        curtains(&mut buf, area, column, INK);
        let clear = Rect::new(
            column.x - CURTAIN_GAP,
            0,
            column.width + 2 * CURTAIN_GAP,
            30,
        );
        assert_eq!(lit(&buf, clear), 0);
        assert!(lit(&buf, Rect::new(0, 0, 8, 27)) > lit(&buf, Rect::new(30, 0, 8, 27)));
        assert!(
            lit(&buf, Rect::new(152, 0, 8, 27)) > 0,
            "the right edge is dithered"
        );
        assert_eq!(
            lit(&buf, Rect::new(0, 27, 160, 3)),
            0,
            "rows below the column stay clear"
        );
    }

    #[test]
    fn curtains_too_narrow_are_left_out() {
        let area = Rect::new(0, 0, 84, 20);
        let mut buf = Buffer::empty(area);
        curtains(&mut buf, area, Rect::new(4, 0, 76, 20), INK);
        assert_eq!(lit(&buf, area), 0);
    }

    #[test]
    fn a_trail_thins_out_toward_the_row_end() {
        let row = Rect::new(0, 0, 64, 1);
        let mut buf = Buffer::empty(row);
        trail(&mut buf, row, INK);
        assert!(lit(&buf, Rect::new(0, 0, 16, 1)) > lit(&buf, Rect::new(48, 0, 16, 1)));
    }

    #[test]
    fn the_end_fade_keeps_to_its_first_rows() {
        let area = Rect::new(0, 0, 80, 20);
        let mut buf = Buffer::empty(area);
        end(&mut buf, area, INK);
        assert!(lit(&buf, Rect::new(0, 0, 80, END_ROWS)) > 0);
        assert_eq!(lit(&buf, Rect::new(0, END_ROWS, 80, 20 - END_ROWS)), 0);
    }

    #[test]
    fn hues_come_from_the_theme_and_greys_have_none() {
        let blue = hue([9, 105, 218]).expect("a blue has a hue");
        assert!((230.0..275.0).contains(&blue), "{blue}");
        let red = hue([217, 106, 77]).expect("a red has a hue");
        assert!(red < 60.0 || red > 340.0, "{red}");
        assert_eq!(hue([128, 128, 128]), None);
    }

    #[test]
    fn each_theme_dithers_in_its_own_accent() {
        // Rex's charcoal-atelier: a terracotta cursor over muted ANSI colors.
        let charcoal = Theme {
            background: [22, 22, 22],
            red: [217, 106, 77],
            green: [154, 171, 132],
            blue: [138, 160, 180],
            accent: [224, 93, 58],
            magenta: [177, 147, 168],
        };
        let ink = Ink::of(charcoal);
        let spread = |c: Rgba| c.r.max(c.g).max(c.b) - c.r.min(c.g).min(c.b);
        assert!(spread(ink.mark) > 40, "clear color: {:?}", ink.mark);
        assert!(
            ink.mark.r > ink.mark.b,
            "warm, like its cursor: {:?}",
            ink.mark
        );
        // merino-light: a blue cursor and a violet magenta.
        let merino = Theme {
            background: [255, 255, 255],
            red: [207, 34, 46],
            green: [17, 99, 41],
            blue: [9, 105, 218],
            accent: [9, 105, 218],
            magenta: [130, 80, 223],
        };
        let ink = Ink::of(merino);
        assert!(
            ink.mark.b > ink.mark.r,
            "blue, like its cursor: {:?}",
            ink.mark
        );
        assert_ne!(ink.mark, ink.accent, "two tones");
    }
}
