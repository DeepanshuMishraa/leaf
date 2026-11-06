//! Ink drawn over a page with the mouse.
//!
//! A stroke is pinned to a character of the page text, the way a highlight
//! is, and its points are measured in cells from that character's cell. When
//! the page is laid out again at another width the stroke moves with the
//! word it was drawn on.

use image::RgbaImage;
use resvg::tiny_skia;

use crate::hue::Hue;

/// A place in cells, from the top-left corner of the anchor's cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Whole cells around a stroke, from the anchor's cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cells {
    pub left: i32,
    pub top: i32,
    pub cols: u16,
    pub rows: u16,
}

impl Cells {
    pub fn right(self) -> i32 {
        self.left + self.cols as i32
    }

    pub fn bottom(self) -> i32 {
        self.top + self.rows as i32
    }

    /// The cells both cover, if any.
    pub fn intersect(self, other: Cells) -> Option<Cells> {
        let (left, top) = (self.left.max(other.left), self.top.max(other.top));
        let (right, bottom) = (
            self.right().min(other.right()),
            self.bottom().min(other.bottom()),
        );
        (right > left && bottom > top).then(|| Cells {
            left,
            top,
            cols: (right - left) as u16,
            rows: (bottom - top) as u16,
        })
    }

    pub fn shift(self, dx: i32, dy: i32) -> Cells {
        Cells {
            left: self.left + dx,
            top: self.top + dy,
            ..self
        }
    }
}

/// One line drawn without letting go of the mouse. `page` is 0-based and
/// `anchor` a character offset into its text, as for highlights.
#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub page: usize,
    pub anchor: usize,
    pub hue: Hue,
    /// Never empty.
    pub points: Vec<Point>,
}

/// The pen's width as a share of a cell's height.
const PEN: f32 = 0.14;
/// Thinnest the pen draws, in pixels.
const MIN_PEN_PX: f32 = 2.0;

fn distance_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / length).clamp(0.0, 1.0)
    };
    let (x, y) = (a.x + t * dx - p.x, a.y + t * dy - p.y);
    (x * x + y * y).sqrt()
}

impl Stroke {
    /// A stroke begun at `at`.
    pub fn begin(page: usize, anchor: usize, hue: Hue, at: Point) -> Stroke {
        Stroke {
            page,
            anchor,
            hue,
            points: vec![at],
        }
    }

    /// Carries the stroke on to `at`; the mouse staying in one cell adds nothing.
    pub fn extend(&mut self, at: Point) -> bool {
        if self.points.last() == Some(&at) {
            return false;
        }
        self.points.push(at);
        true
    }

    /// The cells the stroke's ink falls in.
    pub fn bounds(&self) -> Cells {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in &self.points {
            (x0, y0, x1, y1) = (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y));
        }
        // The pen is thinner than half a cell, so half a cell either side holds it.
        let (left, top) = ((x0 - 0.5).floor() as i32, (y0 - 0.5).floor() as i32);
        let (right, bottom) = ((x1 + 0.5).ceil() as i32, (y1 + 0.5).ceil() as i32);
        Cells {
            left,
            top,
            cols: (right - left).clamp(1, u16::MAX as i32) as u16,
            rows: (bottom - top).clamp(1, u16::MAX as i32) as u16,
        }
    }

    /// Whether the line passes within `reach` cells of `p`.
    pub fn near(&self, p: Point, reach: f32) -> bool {
        match self.points.as_slice() {
            [only] => distance_to_segment(p, *only, *only) <= reach,
            points => points
                .windows(2)
                .any(|w| distance_to_segment(p, w[0], w[1]) <= reach),
        }
    }

    /// The ink inside `clip`, with cells `cell` pixels in size, on a clear background.
    pub fn paint(&self, clip: Cells, cell: (u16, u16)) -> Option<RgbaImage> {
        let (cw, ch) = (cell.0.max(1) as f32, cell.1.max(1) as f32);
        let (width, height) = (clip.cols as u32 * cw as u32, clip.rows as u32 * ch as u32);
        let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
        let px = |p: Point| ((p.x - clip.left as f32) * cw, (p.y - clip.top as f32) * ch);
        let [r, g, b] = self.hue.ink();
        let mut paint = tiny_skia::Paint::default();
        paint.set_color_rgba8(r, g, b, 255);
        paint.anti_alias = true;
        let pen = (ch * PEN).max(MIN_PEN_PX);
        let path = match self.points.as_slice() {
            [] => return None,
            [only] => {
                let (x, y) = px(*only);
                let path = tiny_skia::PathBuilder::from_circle(x, y, pen / 2.0)?;
                pixmap.fill_path(
                    &path,
                    &paint,
                    tiny_skia::FillRule::Winding,
                    tiny_skia::Transform::identity(),
                    None,
                );
                None
            }
            [first, rest @ ..] => {
                // Mouse points come a cell apart; curving through the midpoints smooths the corners.
                let mut pb = tiny_skia::PathBuilder::new();
                let (x, y) = px(*first);
                pb.move_to(x, y);
                for pair in rest.windows(2) {
                    let ((cx, cy), (nx, ny)) = (px(pair[0]), px(pair[1]));
                    pb.quad_to(cx, cy, (cx + nx) / 2.0, (cy + ny) / 2.0);
                }
                if let Some(last) = rest.last() {
                    let (x, y) = px(*last);
                    pb.line_to(x, y);
                }
                pb.finish()
            }
        };
        if let Some(path) = path {
            let stroke = tiny_skia::Stroke {
                width: pen,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..Default::default()
            };
            pixmap.stroke_path(
                &path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
        let pixels = pixmap.pixels().iter().flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        });
        RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
    }

    /// The points as stored: little-endian `f32` pairs.
    pub fn encode_points(&self) -> Vec<u8> {
        self.points
            .iter()
            .flat_map(|p| p.x.to_le_bytes().into_iter().chain(p.y.to_le_bytes()))
            .collect()
    }

    /// Points read back from storage, or `None` for bytes that aren't a stroke we wrote.
    pub fn decode_points(bytes: &[u8]) -> Option<Vec<Point>> {
        let (pairs, rest) = bytes.as_chunks::<8>();
        if pairs.is_empty() || !rest.is_empty() {
            return None;
        }
        pairs
            .iter()
            .map(|&[a, b, c, d, e, f, g, h]| {
                let (x, y) = (
                    f32::from_le_bytes([a, b, c, d]),
                    f32::from_le_bytes([e, f, g, h]),
                );
                (x.is_finite() && y.is_finite()).then_some(Point { x, y })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(points: &[(f32, f32)]) -> Stroke {
        Stroke {
            page: 0,
            anchor: 0,
            hue: Hue::Blue,
            points: points.iter().map(|&(x, y)| Point { x, y }).collect(),
        }
    }

    #[test]
    fn points_survive_storage_and_bad_bytes_are_refused() {
        let stroke = line(&[(0.5, 0.5), (-3.5, 2.5)]);
        assert_eq!(
            Stroke::decode_points(&stroke.encode_points()),
            Some(stroke.points)
        );
        assert_eq!(Stroke::decode_points(&[]), None);
        assert_eq!(Stroke::decode_points(&[0; 7]), None);
        assert_eq!(
            Stroke::decode_points(&f32::NAN.to_le_bytes().repeat(2)),
            None
        );
    }

    #[test]
    fn bounds_hold_every_point_with_room_for_the_pen() {
        let bounds = line(&[(0.5, 0.5), (-3.5, 2.5)]).bounds();
        assert_eq!(
            bounds,
            Cells {
                left: -4,
                top: 0,
                cols: 5,
                rows: 3
            }
        );
    }

    #[test]
    fn near_finds_the_line_between_points_not_only_the_points() {
        let stroke = line(&[(0.5, 0.5), (10.5, 0.5)]);
        assert!(stroke.near(Point { x: 5.5, y: 0.9 }, 0.6));
        assert!(!stroke.near(Point { x: 5.5, y: 2.5 }, 0.6));
    }

    #[test]
    fn paint_inks_the_line_in_its_hue_and_leaves_the_rest_clear() {
        let stroke = line(&[(0.5, 0.5), (3.5, 0.5)]);
        let image = stroke.paint(stroke.bounds(), (10, 20)).unwrap();
        assert_eq!(image.dimensions(), (40, 20));
        let [r, g, b] = Hue::Blue.ink();
        // The line runs through the middle of its row of cells.
        assert_eq!(image.get_pixel(20, 10).0, [r, g, b, 255]);
        assert_eq!(image.get_pixel(20, 2).0[3], 0);
    }

    #[test]
    fn a_clip_paints_only_its_part() {
        let stroke = line(&[(0.5, 0.5), (3.5, 0.5)]);
        let clip = Cells {
            left: 2,
            top: 0,
            cols: 2,
            rows: 1,
        };
        let image = stroke.paint(clip, (10, 20)).unwrap();
        assert_eq!(image.dimensions(), (20, 20));
        assert_eq!(image.get_pixel(5, 10).0[3], 255);
    }

    #[test]
    fn staying_in_a_cell_adds_no_point() {
        let mut stroke = line(&[(0.5, 0.5)]);
        assert!(!stroke.extend(Point { x: 0.5, y: 0.5 }));
        assert!(stroke.extend(Point { x: 1.5, y: 0.5 }));
        assert_eq!(stroke.points.len(), 2);
    }
}
