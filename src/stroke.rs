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
    pub trace: Trace,
}

/// How a stroke's points are joined: curved through, as drawn by hand, or
/// with straight lines, for a shape it was tidied into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trace {
    Freehand,
    Shape,
}

impl Trace {
    pub fn name(self) -> &'static str {
        match self {
            Trace::Freehand => "freehand",
            Trace::Shape => "shape",
        }
    }

    pub fn parse(name: &str) -> Option<Trace> {
        [Trace::Freehand, Trace::Shape]
            .into_iter()
            .find(|t| t.name() == name)
    }
}

/// The pen's width as a share of a cell's height.
const PEN: f32 = 0.22;
/// Thinnest the pen draws, in pixels.
const MIN_PEN_PX: f32 = 3.0;

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

/// The smallest box holding every point of a stroke, in cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Extent {
    pub min: Point,
    pub max: Point,
}

/// A corner of a picked stroke's frame, the handle that resizes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

impl Corner {
    pub const ALL: [Corner; 4] = [
        Corner::TopLeft,
        Corner::TopRight,
        Corner::BottomRight,
        Corner::BottomLeft,
    ];

    fn opposite(self) -> Corner {
        match self {
            Corner::TopLeft => Corner::BottomRight,
            Corner::TopRight => Corner::BottomLeft,
            Corner::BottomRight => Corner::TopLeft,
            Corner::BottomLeft => Corner::TopRight,
        }
    }
}

/// How far a frame stands off the ink it holds, in cells.
pub const FRAME_PAD: Point = Point { x: 0.6, y: 0.4 };
/// The smallest a stroke is resized to, in cells.
const MIN_SIZE: f32 = 1.0;

impl Extent {
    pub fn width(self) -> f32 {
        self.max.x - self.min.x
    }

    pub fn height(self) -> f32 {
        self.max.y - self.min.y
    }

    /// The extent grown by `pad` on every side.
    pub fn padded(self, pad: Point) -> Extent {
        Extent {
            min: Point {
                x: self.min.x - pad.x,
                y: self.min.y - pad.y,
            },
            max: Point {
                x: self.max.x + pad.x,
                y: self.max.y + pad.y,
            },
        }
    }

    pub fn corner(self, corner: Corner) -> Point {
        match corner {
            Corner::TopLeft => self.min,
            Corner::TopRight => Point {
                x: self.max.x,
                y: self.min.y,
            },
            Corner::BottomRight => self.max,
            Corner::BottomLeft => Point {
                x: self.min.x,
                y: self.max.y,
            },
        }
    }

    pub fn contains(self, p: Point) -> bool {
        (self.min.x..=self.max.x).contains(&p.x) && (self.min.y..=self.max.y).contains(&p.y)
    }

    /// The extent with `corner` dragged to `to` and its opposite corner held
    /// still. It can't be turned inside out or squashed smaller than a cell.
    pub fn dragged(self, corner: Corner, to: Point) -> Extent {
        let fixed = self.corner(corner.opposite());
        let grabbed = self.corner(corner);
        // Each side keeps to its own side of the fixed corner.
        let along = |to: f32, grabbed: f32, fixed: f32| {
            if grabbed >= fixed {
                to.max(fixed + MIN_SIZE)
            } else {
                to.min(fixed - MIN_SIZE)
            }
        };
        let moved = Point {
            x: if self.width() > 0.0 {
                along(to.x, grabbed.x, fixed.x)
            } else {
                grabbed.x
            },
            y: if self.height() > 0.0 {
                along(to.y, grabbed.y, fixed.y)
            } else {
                grabbed.y
            },
        };
        Extent {
            min: Point {
                x: moved.x.min(fixed.x),
                y: moved.y.min(fixed.y),
            },
            max: Point {
                x: moved.x.max(fixed.x),
                y: moved.y.max(fixed.y),
            },
        }
    }

    /// The extent grown (or, below 1, shrunk) by `factor` about its middle,
    /// no smaller than a cell.
    pub fn scaled(self, factor: f32) -> Extent {
        let (cx, cy) = (
            (self.min.x + self.max.x) / 2.0,
            (self.min.y + self.max.y) / 2.0,
        );
        let half = |size: f32| {
            if size > 0.0 {
                (size * factor).max(MIN_SIZE) / 2.0
            } else {
                0.0
            }
        };
        let (hw, hh) = (half(self.width()), half(self.height()));
        Extent {
            min: Point {
                x: cx - hw,
                y: cy - hh,
            },
            max: Point {
                x: cx + hw,
                y: cy + hh,
            },
        }
    }
}

/// A picked stroke's frame: a thin box around `extent` with a handle at
/// each corner, inside `clip`, with cells `cell` pixels in size.
pub fn paint_frame(extent: Extent, clip: Cells, cell: (u16, u16)) -> Option<RgbaImage> {
    let (cw, ch) = (cell.0.max(1) as f32, cell.1.max(1) as f32);
    let (width, height) = (clip.cols as u32 * cw as u32, clip.rows as u32 * ch as u32);
    let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
    let px = |p: Point| ((p.x - clip.left as f32) * cw, (p.y - clip.top as f32) * ch);
    let frame = extent.padded(FRAME_PAD);
    let ((x0, y0), (x1, y1)) = (px(frame.min), px(frame.max));
    let mut ink = tiny_skia::Paint::default();
    ink.set_color_rgba8(150, 150, 150, 255);
    ink.anti_alias = true;
    let rect = tiny_skia::Rect::from_ltrb(x0, y0, x1, y1)?;
    let outline = tiny_skia::PathBuilder::from_rect(rect);
    let line = tiny_skia::Stroke {
        width: 1.5,
        dash: tiny_skia::StrokeDash::new(vec![5.0, 4.0], 0.0),
        ..Default::default()
    };
    pixmap.stroke_path(
        &outline,
        &ink,
        &line,
        tiny_skia::Transform::identity(),
        None,
    );
    let mut paper = tiny_skia::Paint::default();
    paper.set_color_rgba8(250, 250, 250, 255);
    let side = (cw * 0.9).max(6.0);
    for corner in Corner::ALL {
        let (x, y) = px(frame.corner(corner));
        let Some(handle) = tiny_skia::Rect::from_xywh(x - side / 2.0, y - side / 2.0, side, side)
        else {
            continue;
        };
        let path = tiny_skia::PathBuilder::from_rect(handle);
        pixmap.fill_path(
            &path,
            &paper,
            tiny_skia::FillRule::Winding,
            tiny_skia::Transform::identity(),
            None,
        );
        pixmap.stroke_path(
            &path,
            &ink,
            &tiny_skia::Stroke {
                width: 1.5,
                ..Default::default()
            },
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

impl Stroke {
    /// The smallest box holding every point.
    pub fn extent(&self) -> Extent {
        let first = self
            .points
            .first()
            .copied()
            .unwrap_or(Point { x: 0.0, y: 0.0 });
        self.points.iter().fold(
            Extent {
                min: first,
                max: first,
            },
            |e, p| Extent {
                min: Point {
                    x: e.min.x.min(p.x),
                    y: e.min.y.min(p.y),
                },
                max: Point {
                    x: e.max.x.max(p.x),
                    y: e.max.y.max(p.y),
                },
            },
        )
    }

    /// The cells the stroke's frame and its handles fall in.
    pub fn frame_bounds(&self) -> Cells {
        let frame = self.extent().padded(FRAME_PAD);
        let (left, top) = (
            (frame.min.x - 1.0).floor() as i32,
            (frame.min.y - 1.0).floor() as i32,
        );
        let (right, bottom) = (
            (frame.max.x + 1.0).ceil() as i32,
            (frame.max.y + 1.0).ceil() as i32,
        );
        Cells {
            left,
            top,
            cols: (right - left).clamp(1, u16::MAX as i32) as u16,
            rows: (bottom - top).clamp(1, u16::MAX as i32) as u16,
        }
    }

    /// Moves every point `dx` cells across and `dy` down.
    pub fn shift(&mut self, dx: f32, dy: f32) {
        for p in &mut self.points {
            p.x += dx;
            p.y += dy;
        }
    }

    /// The stroke stretched so its extent becomes `to`.
    pub fn fitted(&self, to: Extent) -> Stroke {
        let from = self.extent();
        let axis = |v: f32, from_min: f32, from_size: f32, to_min: f32, to_size: f32| {
            if from_size > 0.0 {
                to_min + (v - from_min) * to_size / from_size
            } else {
                v + to_min - from_min
            }
        };
        Stroke {
            points: self
                .points
                .iter()
                .map(|p| Point {
                    x: axis(p.x, from.min.x, from.width(), to.min.x, to.width()),
                    y: axis(p.y, from.min.y, from.height(), to.min.y, to.height()),
                })
                .collect(),
            ..self.clone()
        }
    }

    /// A stroke begun at `at`.
    pub fn begin(page: usize, anchor: usize, hue: Hue, at: Point) -> Stroke {
        Stroke {
            page,
            anchor,
            hue,
            points: vec![at],
            trace: Trace::Freehand,
        }
    }

    /// The stroke as the shape it was meant to be, when it's near one; see
    /// [`crate::shape::tidy`]. `cell` is a cell's size in pixels.
    pub fn tidied(self, cell: (u16, u16)) -> Stroke {
        match crate::shape::tidy(&self.points, cell) {
            Some(points) => Stroke {
                points,
                trace: Trace::Shape,
                ..self
            },
            None => self,
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
            [first, rest @ ..] if self.trace == Trace::Shape => {
                let mut pb = tiny_skia::PathBuilder::new();
                let (x, y) = px(*first);
                pb.move_to(x, y);
                for p in rest {
                    let (x, y) = px(*p);
                    pb.line_to(x, y);
                }
                // A shape that ends where it began is closed, so its first corner is sharp too.
                if rest.last() == Some(first) {
                    pb.close();
                }
                pb.finish()
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
            trace: Trace::Freehand,
        }
    }

    #[test]
    fn a_rough_box_is_tidied_into_a_shape_and_a_scribble_is_left_alone() {
        let mut path = vec![];
        for x in 0..=12 {
            path.push((x as f32 + 0.5, 0.5));
        }
        for y in 1..=6 {
            path.push((12.5, y as f32 + 0.5));
        }
        for x in (0..12).rev() {
            path.push((x as f32 + 0.5, 6.5));
        }
        for y in (0..6).rev() {
            path.push((0.5, y as f32 + 0.5));
        }
        let boxed = line(&path).tidied((8, 16));
        assert_eq!(boxed.trace, Trace::Shape);
        assert_eq!(boxed.points.len(), 5);
        let scribble = line(&[(0.5, 0.5), (1.5, 2.5), (2.5, 0.5), (3.5, 2.5)]);
        assert_eq!(scribble.clone().tidied((8, 16)), scribble);
    }

    fn extent(x0: f32, y0: f32, x1: f32, y1: f32) -> Extent {
        Extent {
            min: Point { x: x0, y: y0 },
            max: Point { x: x1, y: y1 },
        }
    }

    #[test]
    fn dragging_a_corner_holds_the_opposite_one_still() {
        let e = extent(2.0, 2.0, 10.0, 6.0);
        assert_eq!(
            e.dragged(Corner::BottomRight, Point { x: 14.0, y: 9.0 }),
            extent(2.0, 2.0, 14.0, 9.0)
        );
        assert_eq!(
            e.dragged(Corner::TopLeft, Point { x: 0.0, y: 1.0 }),
            extent(0.0, 1.0, 10.0, 6.0)
        );
        // Dragged past the fixed corner, it stops a cell short instead of flipping.
        assert_eq!(
            e.dragged(Corner::TopRight, Point { x: -5.0, y: 20.0 }),
            extent(2.0, 5.0, 3.0, 6.0)
        );
    }

    #[test]
    fn a_flat_line_resizes_along_its_length_only() {
        let e = extent(2.0, 3.0, 10.0, 3.0);
        assert_eq!(
            e.dragged(Corner::BottomRight, Point { x: 20.0, y: 9.0 }),
            extent(2.0, 3.0, 20.0, 3.0)
        );
    }

    #[test]
    fn scaling_grows_about_the_middle_but_not_below_a_cell() {
        assert_eq!(
            extent(0.0, 0.0, 10.0, 4.0).scaled(1.5),
            extent(-2.5, -1.0, 12.5, 5.0)
        );
        assert_eq!(
            extent(0.0, 0.0, 2.0, 2.0).scaled(0.1),
            extent(0.5, 0.5, 1.5, 1.5)
        );
    }

    #[test]
    fn fitting_stretches_every_point_into_the_new_extent() {
        let stroke = line(&[(0.0, 0.0), (4.0, 2.0), (2.0, 1.0)]);
        let fitted = stroke.fitted(extent(10.0, 10.0, 18.0, 16.0));
        assert_eq!(fitted.extent(), extent(10.0, 10.0, 18.0, 16.0));
        assert_eq!(fitted.points[2], Point { x: 14.0, y: 13.0 });
        let mut moved = stroke.clone();
        moved.shift(1.0, -1.0);
        assert_eq!(moved.extent(), extent(1.0, -1.0, 5.0, 1.0));
    }

    #[test]
    fn a_frame_has_its_handles_inside_its_bounds() {
        let stroke = line(&[(0.5, 0.5), (6.5, 3.5)]);
        let bounds = stroke.frame_bounds();
        let image = paint_frame(stroke.extent(), bounds, (10, 20)).unwrap();
        assert_eq!(
            image.dimensions(),
            (bounds.cols as u32 * 10, bounds.rows as u32 * 20)
        );
        // The top-left handle, at the padded corner, is drawn.
        let corner = stroke.extent().padded(FRAME_PAD).min;
        let (x, y) = (
            ((corner.x - bounds.left as f32) * 10.0) as u32,
            ((corner.y - bounds.top as f32) * 20.0) as u32,
        );
        assert_eq!(image.get_pixel(x, y).0[3], 255);
    }

    #[test]
    fn a_shape_is_drawn_with_sharp_corners() {
        let mut square = line(&[(0.5, 0.5), (4.5, 0.5), (4.5, 2.5), (0.5, 2.5), (0.5, 0.5)]);
        let cell = (10, 20);
        let curved = square.paint(square.bounds(), cell).unwrap();
        square.trace = Trace::Shape;
        let sharp = square.paint(square.bounds(), cell).unwrap();
        // The top-right corner, at (45, 10): inked when joined straight, cut off when curved.
        assert_eq!(sharp.get_pixel(45, 10).0[3], 255);
        assert!(curved.get_pixel(45, 10).0[3] < 255);
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
