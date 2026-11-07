//! Tidies a freehand stroke into the shape it was meant to be: a straight
//! line, a triangle, a rectangle (or other four-sided shape), an ellipse or
//! a circle. Anything else, such as handwriting, stays as drawn.
//!
//! The mouse only reports which cell it's in, and cells are taller than they
//! are wide, so shapes are judged in pixels, not cells.

use std::f32::consts::{PI, TAU};

use crate::stroke::Point;

/// A line within this many degrees of flat or upright is snapped to it.
const SNAP_DEGREES: f32 = 8.0;
/// A turn gentler than this at a corner means it isn't one.
const MIN_CORNER_DEGREES: f32 = 30.0;
/// Points around a tidied ellipse.
const ELLIPSE_POINTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Px {
    x: f32,
    y: f32,
}

impl Px {
    fn distance(self, other: Px) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

/// How far `p` is from the segment `a`–`b`.
fn off_segment(p: Px, a: Px, b: Px) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    if length == 0.0 {
        return p.distance(a);
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / length).clamp(0.0, 1.0);
    p.distance(Px {
        x: a.x + t * dx,
        y: a.y + t * dy,
    })
}

/// The few points that keep the path within `tolerance` of where it was
/// (Ramer–Douglas–Peucker).
fn simplify(path: &[Px], tolerance: f32) -> Vec<Px> {
    let (Some(&first), Some(&last)) = (path.first(), path.last()) else {
        return Vec::new();
    };
    let farthest = path
        .iter()
        .enumerate()
        .map(|(i, &p)| (i, off_segment(p, first, last)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    match farthest {
        Some((i, d)) if d > tolerance && i > 0 && i + 1 < path.len() => {
            let mut left = simplify(&path[..=i], tolerance);
            left.pop();
            left.extend(simplify(&path[i..], tolerance));
            left
        }
        _ if path.len() > 1 => vec![first, last],
        _ => vec![first],
    }
}

/// How sharply the path turns at `b` on its way from `a` to `c`, in degrees.
fn turn(a: Px, b: Px, c: Px) -> f32 {
    let (u, v) = ((b.x - a.x, b.y - a.y), (c.x - b.x, c.y - b.y));
    let angle = (u.0 * v.1 - u.1 * v.0).atan2(u.0 * v.0 + u.1 * v.1);
    angle.abs().to_degrees()
}

/// The corners of a closed path: simplified, then with points that are
/// barely a turn, or crowd a neighbor, dropped.
fn corners(path: &[Px], size: f32) -> Vec<Px> {
    let diagonal = size * std::f32::consts::SQRT_2;
    let mut corners = simplify(path, diagonal * 0.07);
    // The path ends where it began, so the last point is the first again.
    corners.pop();
    loop {
        let n = corners.len();
        if n <= 3 {
            return corners;
        }
        let weakest = (0..n)
            .map(|i| {
                let (a, b, c) = (corners[(i + n - 1) % n], corners[i], corners[(i + 1) % n]);
                let crowded = b.distance(a).min(b.distance(c)) < size * 0.15;
                (i, if crowded { 0.0 } else { turn(a, b, c) })
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        match weakest {
            Some((i, degrees)) if degrees < MIN_CORNER_DEGREES => {
                corners.remove(i);
            }
            _ => return corners,
        }
    }
}

struct Bounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Bounds {
    fn of(path: &[Px]) -> Bounds {
        path.iter().fold(
            Bounds {
                left: f32::MAX,
                top: f32::MAX,
                right: f32::MIN,
                bottom: f32::MIN,
            },
            |b, p| Bounds {
                left: b.left.min(p.x),
                top: b.top.min(p.y),
                right: b.right.max(p.x),
                bottom: b.bottom.max(p.y),
            },
        )
    }

    fn width(&self) -> f32 {
        self.right - self.left
    }

    fn height(&self) -> f32 {
        self.bottom - self.top
    }

    fn corners(&self) -> [Px; 4] {
        [
            Px {
                x: self.left,
                y: self.top,
            },
            Px {
                x: self.right,
                y: self.top,
            },
            Px {
                x: self.right,
                y: self.bottom,
            },
            Px {
                x: self.left,
                y: self.bottom,
            },
        ]
    }
}

/// A straight line from the first point to the last, if the path keeps close to one.
fn straight(path: &[Px], first: Px, last: Px, cell_h: f32) -> Option<Vec<Px>> {
    let chord = first.distance(last);
    let wobble = path
        .iter()
        .map(|&p| off_segment(p, first, last))
        .fold(0.0, f32::max);
    if wobble > (cell_h * 0.6).max(chord * 0.08) {
        return None;
    }
    let degrees = (last.y - first.y)
        .atan2(last.x - first.x)
        .to_degrees()
        .abs();
    let flat = degrees.min(180.0 - degrees);
    let end = if flat <= SNAP_DEGREES {
        Px {
            x: last.x,
            y: first.y,
        }
    } else if (90.0 - degrees).abs() <= SNAP_DEGREES {
        Px {
            x: first.x,
            y: last.y,
        }
    } else {
        last
    };
    Some(vec![first, end])
}

/// How far the path strays from the ellipse filling its bounds, on average,
/// as a share of the radius.
fn ellipse_misfit(path: &[Px], bounds: &Bounds) -> f32 {
    let (rx, ry) = (bounds.width() / 2.0, bounds.height() / 2.0);
    let (cx, cy) = (bounds.left + rx, bounds.top + ry);
    let total: f32 = path
        .iter()
        .map(|p| (((p.x - cx) / rx).hypot((p.y - cy) / ry) - 1.0).abs())
        .sum();
    total / path.len() as f32
}

/// The ellipse filling `bounds`, made a circle when it's nearly round.
fn ellipse(bounds: &Bounds) -> Vec<Px> {
    let (mut rx, mut ry) = (bounds.width() / 2.0, bounds.height() / 2.0);
    let (cx, cy) = (bounds.left + rx, bounds.top + ry);
    if (rx - ry).abs() <= rx.max(ry) * 0.15 {
        let r = (rx + ry) / 2.0;
        (rx, ry) = (r, r);
    }
    (0..=ELLIPSE_POINTS)
        .map(|i| {
            // Starting at the top, like a pen would.
            let a = i as f32 / ELLIPSE_POINTS as f32 * TAU - PI / 2.0;
            Px {
                x: cx + rx * a.cos(),
                y: cy + ry * a.sin(),
            }
        })
        .collect()
}

/// A four-cornered shape: its bounds when each corner sits near a different
/// corner of them, otherwise the corners joined as drawn.
fn four_sided(corners: &[Px], bounds: &Bounds) -> Vec<Px> {
    let reach = bounds.width().min(bounds.height()) * 0.3;
    let boxed = bounds.corners();
    let near: Vec<Option<usize>> = corners
        .iter()
        .map(|c| boxed.iter().position(|b| c.distance(*b) <= reach))
        .collect();
    let mut matched: Vec<usize> = near.iter().flatten().copied().collect();
    matched.sort_unstable();
    matched.dedup();
    let shape: Vec<Px> = if matched.len() == 4 {
        boxed.to_vec()
    } else {
        corners.to_vec()
    };
    let mut closed = shape.clone();
    closed.extend(shape.first());
    closed
}

/// The shape a stroke was meant to be, as points in the same cells to be
/// joined with straight lines; `None` when it isn't near enough to one and
/// should stay as drawn. `cell` is a cell's size in pixels.
pub fn tidy(points: &[Point], cell: (u16, u16)) -> Option<Vec<Point>> {
    let (cw, ch) = (cell.0.max(1) as f32, cell.1.max(1) as f32);
    let path: Vec<Px> = points
        .iter()
        .map(|p| Px {
            x: p.x * cw,
            y: p.y * ch,
        })
        .collect();
    let (&first, &last) = (path.first()?, path.last()?);
    if path.len() < 3 {
        return None;
    }
    let bounds = Bounds::of(&path);
    let size = bounds.width().max(bounds.height());
    // Dots and dashes are too small to judge.
    if size < ch * 1.5 {
        return None;
    }
    let length: f32 = path.windows(2).map(|w| w[0].distance(w[1])).sum();
    let closed = first.distance(last) <= size * 0.3 && length > size * 2.5;
    let shape = if !closed {
        straight(&path, first, last, ch)?
    } else if bounds.width() < ch || bounds.height() < ch {
        // A loop squashed flat is a line drawn there and back.
        return None;
    } else {
        let misfit = ellipse_misfit(&path, &bounds);
        let corners = corners(&path, size);
        match corners.len() {
            _ if misfit < 0.1 => ellipse(&bounds),
            3 => {
                let mut triangle = corners.clone();
                triangle.extend(corners.first());
                triangle
            }
            4 => four_sided(&corners, &bounds),
            _ if misfit < 0.18 => ellipse(&bounds),
            _ => return None,
        }
    };
    Some(
        shape
            .into_iter()
            .map(|p| Point {
                x: p.x / cw,
                y: p.y / ch,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: (u16, u16) = (8, 16);

    /// The cells a mouse passes through going straight from `a` to `b`, as cell middles.
    fn drag(a: (i32, i32), b: (i32, i32)) -> Vec<Point> {
        let steps = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
        (0..=steps)
            .map(|i| {
                let t = i as f32 / steps as f32;
                Point {
                    x: (a.0 as f32 + t * (b.0 - a.0) as f32).round() + 0.5,
                    y: (a.1 as f32 + t * (b.1 - a.1) as f32).round() + 0.5,
                }
            })
            .collect()
    }

    /// A path through `corners`, cell by cell.
    fn through(corners: &[(i32, i32)]) -> Vec<Point> {
        let mut path = Vec::new();
        for pair in corners.windows(2) {
            let mut leg = drag(pair[0], pair[1]);
            if !path.is_empty() {
                leg.remove(0);
            }
            path.extend(leg);
        }
        path
    }

    fn pixels(points: &[Point]) -> Vec<(f32, f32)> {
        points
            .iter()
            .map(|p| (p.x * CELL.0 as f32, p.y * CELL.1 as f32))
            .collect()
    }

    #[test]
    fn a_wobbly_flat_line_comes_out_straight_and_level() {
        let mut path = drag((0, 0), (20, 0));
        path[7].y += 0.4;
        path[12].y -= 0.3;
        path.last_mut().unwrap().y += 0.6;
        let line = tidy(&path, CELL).unwrap();
        assert_eq!(line.len(), 2);
        assert_eq!(line[0].y, line[1].y);
        assert_eq!(line[1].x, 20.5);
    }

    #[test]
    fn a_staircase_diagonal_is_one_straight_line() {
        let line = tidy(&drag((0, 0), (16, 8)), CELL).unwrap();
        assert_eq!(
            line,
            vec![Point { x: 0.5, y: 0.5 }, Point { x: 16.5, y: 8.5 }]
        );
    }

    #[test]
    fn a_rough_box_becomes_its_rectangle() {
        let mut path = through(&[(2, 1), (20, 1), (21, 8), (1, 9), (2, 1)]);
        path.push(Point { x: 3.5, y: 1.5 });
        let rect = pixels(&tidy(&path, CELL).unwrap());
        assert_eq!(rect.len(), 5);
        assert_eq!(rect[0], rect[4]);
        // Two lefts, two rights, two tops, two bottoms: an upright rectangle.
        let xs: Vec<f32> = rect[..4].iter().map(|p| p.0).collect();
        let ys: Vec<f32> = rect[..4].iter().map(|p| p.1).collect();
        assert_eq!(xs[0], xs[3]);
        assert_eq!(xs[1], xs[2]);
        assert_eq!(ys[0], ys[1]);
        assert_eq!(ys[2], ys[3]);
    }

    #[test]
    fn a_box_started_halfway_along_a_side_still_has_four_corners() {
        let path = through(&[(10, 1), (20, 1), (20, 8), (1, 8), (1, 1), (9, 1)]);
        assert_eq!(tidy(&path, CELL).unwrap().len(), 5);
    }

    #[test]
    fn a_rough_triangle_keeps_its_three_corners() {
        let path = through(&[(10, 0), (20, 9), (0, 9), (10, 0)]);
        let triangle = tidy(&path, CELL).unwrap();
        assert_eq!(triangle.len(), 4);
        assert_eq!(triangle[0], triangle[3]);
    }

    /// A loop round an ellipse of `rx` × `ry` pixels, through the cells it crosses.
    fn ring(rx: f32, ry: f32, wobble: f32) -> Vec<Point> {
        let mut path: Vec<Point> = Vec::new();
        for i in 0..=120 {
            let a = i as f32 / 120.0 * TAU;
            let r = 1.0 + wobble * (a * 5.0).sin();
            let p = Point {
                x: ((rx * r * a.cos() + 200.0) / CELL.0 as f32).floor() + 0.5,
                y: ((ry * r * a.sin() + 200.0) / CELL.1 as f32).floor() + 0.5,
            };
            if path.last() != Some(&p) {
                path.push(p);
            }
        }
        path
    }

    #[test]
    fn a_wobbly_round_loop_becomes_a_true_circle() {
        let circle = pixels(&tidy(&ring(80.0, 80.0, 0.06), CELL).unwrap());
        assert_eq!(circle.len(), ELLIPSE_POINTS + 1);
        let (xs, ys): (Vec<f32>, Vec<f32>) = circle.iter().copied().unzip();
        let width = xs.iter().copied().fold(f32::MIN, f32::max)
            - xs.iter().copied().fold(f32::MAX, f32::min);
        let height = ys.iter().copied().fold(f32::MIN, f32::max)
            - ys.iter().copied().fold(f32::MAX, f32::min);
        assert!((width - height).abs() < 0.5, "{width} × {height}");
    }

    #[test]
    fn round_loops_of_any_size_and_shakiness_are_ellipses() {
        for r in [40.0, 60.0, 90.0, 130.0, 180.0] {
            for wobble in [0.0, 0.03, 0.06, 0.09] {
                for stretch in [0.5, 1.0, 1.6] {
                    let path = ring(r * stretch, r, wobble);
                    let tidied = tidy(&path, CELL).map(|s| s.len());
                    assert_eq!(
                        tidied,
                        Some(ELLIPSE_POINTS + 1),
                        "r {r} wobble {wobble} stretch {stretch}"
                    );
                }
            }
        }
    }

    #[test]
    fn boxes_of_any_size_are_rectangles() {
        for (w, h) in [(4, 3), (8, 2), (12, 6), (30, 12), (50, 20)] {
            let path = through(&[(1, 1), (1 + w, 1), (1 + w, 1 + h), (1, 1 + h), (1, 1)]);
            assert_eq!(tidy(&path, CELL).map(|s| s.len()), Some(5), "{w} × {h}");
        }
    }

    #[test]
    fn an_oval_stays_an_oval() {
        let oval = pixels(&tidy(&ring(120.0, 50.0, 0.03), CELL).unwrap());
        assert_eq!(oval.len(), ELLIPSE_POINTS + 1);
        let (xs, ys): (Vec<f32>, Vec<f32>) = oval.iter().copied().unzip();
        let width = xs.iter().copied().fold(f32::MIN, f32::max)
            - xs.iter().copied().fold(f32::MAX, f32::min);
        let height = ys.iter().copied().fold(f32::MIN, f32::max)
            - ys.iter().copied().fold(f32::MAX, f32::min);
        assert!(width > height * 1.8);
    }

    #[test]
    fn handwriting_and_ticks_stay_as_drawn() {
        let squiggle = through(&[(0, 4), (3, 0), (6, 6), (9, 0), (12, 6), (15, 1)]);
        assert_eq!(tidy(&squiggle, CELL), None);
        let tick = through(&[(0, 3), (2, 5), (8, 0)]);
        assert_eq!(tidy(&tick, CELL), None);
        assert_eq!(
            tidy(&drag((0, 0), (1, 0)), CELL),
            None,
            "too small to judge"
        );
    }
}
