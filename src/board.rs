//! A whiteboard: boxes, ellipses, diamonds, arrows, lines, ink, text,
//! pictures and frames on an endless canvas, kept as an Excalidraw file so
//! excalidraw.com and agents can read and write the same board. Lengths are
//! Excalidraw pixels, with y growing down; angles are radians, clockwise.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

/// The ink a new element gets when no color is picked: Excalidraw's own.
pub const INK: &str = "#1e1e1e";

#[derive(Debug, thiserror::Error)]
pub enum BoardFailed {
    #[error("Couldn't read the board {path} ({cause}). Nothing was changed.")]
    Read { path: String, cause: std::io::Error },
    #[error(
        "{path} isn't an Excalidraw board ({cause}). Nothing was changed; fix the file or open another path."
    )]
    Parse { path: String, cause: String },
    #[error(
        "Couldn't save the board to {path} ({cause}). The board is still open with your changes; try again once the file can be written."
    )]
    Write { path: String, cause: std::io::Error },
    #[error(
        "Couldn't start a new board at {path} ({cause}). Check that the folder can be written, or name a board file to open."
    )]
    Create { path: String, cause: std::io::Error },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

impl Pt {
    pub fn new(x: f64, y: f64) -> Self {
        Pt { x, y }
    }

    pub fn plus(self, other: Pt) -> Pt {
        Pt::new(self.x + other.x, self.y + other.y)
    }

    pub fn minus(self, other: Pt) -> Pt {
        Pt::new(self.x - other.x, self.y - other.y)
    }

    /// Turned by `angle` around `center`.
    pub fn turn(self, center: Pt, angle: f64) -> Pt {
        if angle == 0.0 {
            return self;
        }
        let (sin, cos) = angle.sin_cos();
        let d = self.minus(center);
        Pt::new(
            center.x + d.x * cos - d.y * sin,
            center.y + d.x * sin + d.y * cos,
        )
    }

    pub fn distance(self, other: Pt) -> f64 {
        let d = self.minus(other);
        (d.x * d.x + d.y * d.y).sqrt()
    }
}

/// An upright rectangle: left, top, right and bottom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl Frame {
    pub fn around(a: Pt, b: Pt) -> Frame {
        Frame {
            x0: a.x.min(b.x),
            y0: a.y.min(b.y),
            x1: a.x.max(b.x),
            y1: a.y.max(b.y),
        }
    }

    pub fn width(self) -> f64 {
        self.x1 - self.x0
    }

    pub fn height(self) -> f64 {
        self.y1 - self.y0
    }

    pub fn center(self) -> Pt {
        Pt::new((self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0)
    }

    pub fn union(self, other: Frame) -> Frame {
        Frame {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn contains(self, other: Frame) -> bool {
        other.x0 >= self.x0 && other.x1 <= self.x1 && other.y0 >= self.y0 && other.y1 <= self.y1
    }

    pub fn grow(self, by: f64) -> Frame {
        Frame {
            x0: self.x0 - by,
            y0: self.y0 - by,
            x1: self.x1 + by,
            y1: self.y1 + by,
        }
    }

    /// Where `p` lands when this frame is stretched onto `to`.
    pub fn map(self, p: Pt, to: Frame) -> Pt {
        let axis = |v: f64, a0: f64, a: f64, b0: f64, b: f64| {
            if a.abs() < f64::EPSILON {
                v - a0 + b0
            } else {
                b0 + (v - a0) / a * b
            }
        };
        Pt::new(
            axis(p.x, self.x0, self.width(), to.x0, to.width()),
            axis(p.y, self.y0, self.height(), to.y0, to.height()),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FillStyle {
    Hachure,
    CrossHatch,
    Solid,
}

impl FillStyle {
    fn parse(name: &str) -> FillStyle {
        match name {
            "solid" => FillStyle::Solid,
            "cross-hatch" => FillStyle::CrossHatch,
            _ => FillStyle::Hachure,
        }
    }

    fn name(self) -> &'static str {
        match self {
            FillStyle::Hachure => "hachure",
            FillStyle::CrossHatch => "cross-hatch",
            FillStyle::Solid => "solid",
        }
    }
}

/// What's painted inside a closed shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub color: String,
    pub style: FillStyle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dash {
    Solid,
    Dashed,
    Dotted,
}

impl Dash {
    fn parse(name: &str) -> Dash {
        match name {
            "dashed" => Dash::Dashed,
            "dotted" => Dash::Dotted,
            _ => Dash::Solid,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Dash::Solid => "solid",
            Dash::Dashed => "dashed",
            Dash::Dotted => "dotted",
        }
    }
}

/// One end of an arrow or line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Start,
    End,
}

impl End {
    fn key(self, what: &str) -> String {
        match self {
            End::Start => format!("start{what}"),
            End::End => format!("end{what}"),
        }
    }
}

/// The mark at the end of an arrow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Arrow,
    Triangle,
    Diamond,
    Bar,
    Dot,
}

impl Head {
    fn parse(name: &str) -> Head {
        match name {
            "triangle" | "triangle_outline" => Head::Triangle,
            "diamond" | "diamond_outline" => Head::Diamond,
            "bar" => Head::Bar,
            "dot" | "circle" | "circle_outline" => Head::Dot,
            _ => Head::Arrow,
        }
    }
}

/// A head placed on the board: where its tip is and which way it points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tip {
    pub head: Head,
    pub at: Pt,
    pub angle: f64,
    pub size: f64,
}

impl Tip {
    /// The head drawn as open polylines, and whether it's solid inside.
    pub fn paths(self) -> (Vec<Vec<Pt>>, bool) {
        let along = |angle: f64, length: f64| {
            Pt::new(
                self.at.x + length * angle.cos(),
                self.at.y + length * angle.sin(),
            )
        };
        let back = self.angle + std::f64::consts::PI;
        match self.head {
            Head::Arrow => (
                vec![
                    vec![along(back + 0.45, self.size), self.at],
                    vec![along(back - 0.45, self.size), self.at],
                ],
                false,
            ),
            Head::Triangle => (
                vec![vec![
                    self.at,
                    along(back + 0.4, self.size),
                    along(back - 0.4, self.size),
                    self.at,
                ]],
                true,
            ),
            Head::Diamond => (
                vec![vec![
                    self.at,
                    along(back + 0.45, self.size * 0.6),
                    along(back, self.size * 1.1),
                    along(back - 0.45, self.size * 0.6),
                    self.at,
                ]],
                true,
            ),
            Head::Bar => (
                vec![vec![
                    along(self.angle + std::f64::consts::FRAC_PI_2, self.size / 2.0),
                    along(self.angle - std::f64::consts::FRAC_PI_2, self.size / 2.0),
                ]],
                false,
            ),
            Head::Dot => {
                let r = self.size / 4.0;
                let center = along(back, r);
                (
                    vec![
                        (0..=12)
                            .map(|i| {
                                let a = f64::from(i) / 12.0 * std::f64::consts::TAU;
                                Pt::new(center.x + r * a.cos(), center.y + r * a.sin())
                            })
                            .collect(),
                    ],
                    true,
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Rectangle {
        w: f64,
        h: f64,
    },
    Ellipse {
        w: f64,
        h: f64,
    },
    Diamond {
        w: f64,
        h: f64,
    },
    /// Points from the element's origin, the first at (0, 0).
    Line {
        points: Vec<Pt>,
    },
    Arrow {
        points: Vec<Pt>,
    },
    Draw {
        points: Vec<Pt>,
    },
    Text {
        text: String,
        size: f64,
    },
    /// A picture whose pixels are in the file's `files`, by id.
    Image {
        w: f64,
        h: f64,
        file: String,
    },
    /// A named area that holds other elements.
    Frame {
        w: f64,
        h: f64,
        name: String,
    },
    /// A kind the board doesn't draw (an embed…), kept as it was and shown
    /// as its outline.
    Other {
        kind: String,
        w: f64,
        h: f64,
    },
}

impl Shape {
    pub fn kind(&self) -> &str {
        match self {
            Shape::Rectangle { .. } => "rectangle",
            Shape::Ellipse { .. } => "ellipse",
            Shape::Diamond { .. } => "diamond",
            Shape::Line { .. } => "line",
            Shape::Arrow { .. } => "arrow",
            Shape::Draw { .. } => "freedraw",
            Shape::Text { .. } => "text",
            Shape::Image { .. } => "image",
            Shape::Frame { .. } => "frame",
            Shape::Other { kind, .. } => kind,
        }
    }

    /// Whether arrows attach to it and text can be written inside it.
    pub fn holds(&self) -> bool {
        matches!(
            self,
            Shape::Rectangle { .. } | Shape::Ellipse { .. } | Shape::Diamond { .. }
        )
    }
}

/// How wide one character of text is, as a share of its size.
pub const ADVANCE: f64 = 0.6;
/// How far apart lines of text are, as a share of its size.
pub const LEADING: f64 = 1.25;

/// Width and height of text at `size`, roughly as Excalidraw measures it.
pub fn text_size(text: &str, size: f64) -> (f64, f64) {
    let lines = text.split('\n');
    let widest = lines.clone().map(|l| l.chars().count()).max().unwrap_or(0);
    (
        widest as f64 * size * ADVANCE,
        lines.count().max(1) as f64 * size * LEADING,
    )
}

fn extent(points: &[Pt]) -> Frame {
    points.iter().fold(
        Frame {
            x0: f64::MAX,
            y0: f64::MAX,
            x1: f64::MIN,
            y1: f64::MIN,
        },
        |f, p| Frame {
            x0: f.x0.min(p.x),
            y0: f.y0.min(p.y),
            x1: f.x1.max(p.x),
            y1: f.y1.max(p.y),
        },
    )
}

pub fn distance_to_segment(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    let t = if length == 0.0 {
        0.0
    } else {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / length).clamp(0.0, 1.0)
    };
    p.distance(Pt::new(a.x + t * dx, a.y + t * dy))
}

/// Where segment `a`–`b` crosses segment `c`–`d`.
fn crossing(a: Pt, b: Pt, c: Pt, d: Pt) -> Option<Pt> {
    let (r, s) = (b.minus(a), d.minus(c));
    let denominator = r.x * s.y - r.y * s.x;
    if denominator.abs() < f64::EPSILON {
        return None;
    }
    let ac = c.minus(a);
    let t = (ac.x * s.y - ac.y * s.x) / denominator;
    let u = (ac.x * r.y - ac.y * r.x) / denominator;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u))
        .then(|| Pt::new(a.x + t * r.x, a.y + t * r.y))
}

/// Whether `p` is inside the closed polygon `points`.
fn inside(p: Pt, points: &[Pt]) -> bool {
    let mut inside = false;
    let mut j = points.len().wrapping_sub(1);
    for (i, a) in points.iter().enumerate() {
        let b = points[j];
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    pub id: String,
    pub x: f64,
    pub y: f64,
    /// Turned clockwise around its middle, in radians.
    pub angle: f64,
    /// A CSS color, `#rrggbb` when leaf picked it.
    pub color: String,
    pub fill: Option<Fill>,
    pub dash: Dash,
    pub stroke: f64,
    /// Innermost first, as Excalidraw keeps them.
    pub groups: Vec<String>,
    /// For text written inside a shape, that shape's id.
    pub container: Option<String>,
    pub shape: Shape,
    /// Every field as read, so ones leaf doesn't use survive a save.
    raw: Map<String, Value>,
}

impl Element {
    pub fn new(shape: Shape, at: Pt, color: &str) -> Self {
        let mut raw = Map::new();
        let defaults = json!({
            "roughness": 1,
            "opacity": 100,
            "frameId": null,
            "roundness": null,
            "seed": random() % 2_000_000_000,
            "version": 1,
            "versionNonce": random() % 2_000_000_000,
            "isDeleted": false,
            "boundElements": null,
            "link": null,
            "locked": false,
        });
        let extra = match &shape {
            Shape::Text { .. } => json!({
                "fontFamily": 1,
                "textAlign": "left",
                "verticalAlign": "top",
                "lineHeight": LEADING,
                "autoResize": true,
            }),
            Shape::Line { .. } | Shape::Arrow { .. } => json!({
                "lastCommittedPoint": null,
                "startBinding": null,
                "endBinding": null,
                "startArrowhead": null,
                "endArrowhead": if matches!(shape, Shape::Arrow { .. }) { json!("arrow") } else { Value::Null },
            }),
            Shape::Draw { .. } => json!({ "pressures": [], "simulatePressure": true }),
            _ => json!({}),
        };
        for value in [defaults, extra] {
            if let Value::Object(map) = value {
                raw.extend(map);
            }
        }
        Element {
            id: new_id(),
            x: at.x,
            y: at.y,
            angle: 0.0,
            color: color.to_owned(),
            fill: None,
            dash: Dash::Solid,
            stroke: 2.0,
            groups: Vec::new(),
            container: None,
            shape,
            raw,
        }
    }

    /// An element of an Excalidraw file; `None` for a deleted one, and an
    /// error for a record that isn't an element at all.
    fn from_value(value: Value) -> Result<Option<Self>, String> {
        let Value::Object(raw) = value else {
            return Err(format!("{value} isn't an element object"));
        };
        if raw.get("isDeleted").and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }
        let number = |key: &str| raw.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let string = |key: &str| raw.get(key).and_then(Value::as_str).unwrap_or_default();
        let points = || {
            raw.get("points")
                .and_then(Value::as_array)
                .map(|points| {
                    points
                        .iter()
                        .filter_map(|p| Some(Pt::new(p.get(0)?.as_f64()?, p.get(1)?.as_f64()?)))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let (w, h) = (number("width"), number("height"));
        let Some(kind) = raw.get("type").and_then(Value::as_str).map(str::to_owned) else {
            let id = raw
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("without an id");
            return Err(format!("the element {id} has no \"type\""));
        };
        let shape = match kind.as_str() {
            "rectangle" => Shape::Rectangle { w, h },
            "ellipse" => Shape::Ellipse { w, h },
            "diamond" => Shape::Diamond { w, h },
            "line" => Shape::Line { points: points() },
            "arrow" => Shape::Arrow { points: points() },
            "freedraw" => Shape::Draw { points: points() },
            "text" => Shape::Text {
                text: string("text").to_owned(),
                size: raw.get("fontSize").and_then(Value::as_f64).unwrap_or(20.0),
            },
            "image" => Shape::Image {
                w,
                h,
                file: string("fileId").to_owned(),
            },
            "frame" | "magicframe" => Shape::Frame {
                w,
                h,
                name: string("name").to_owned(),
            },
            _ => Shape::Other { kind, w, h },
        };
        let background = string("backgroundColor");
        let fill = (!background.is_empty() && background != "transparent").then(|| Fill {
            color: background.to_owned(),
            style: FillStyle::parse(string("fillStyle")),
        });
        Ok(Some(Element {
            id: raw
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(new_id, str::to_owned),
            x: number("x"),
            y: number("y"),
            angle: number("angle"),
            color: raw
                .get("strokeColor")
                .and_then(Value::as_str)
                .unwrap_or(INK)
                .to_owned(),
            fill,
            dash: Dash::parse(string("strokeStyle")),
            stroke: raw
                .get("strokeWidth")
                .and_then(Value::as_f64)
                .unwrap_or(2.0),
            groups: raw
                .get("groupIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            container: raw
                .get("containerId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            shape,
            raw,
        }))
    }

    fn to_value(&self) -> Value {
        let mut raw = self.raw.clone();
        let frame = self.frame();
        let mut set = |key: &str, value: Value| {
            raw.insert(key.to_owned(), value);
        };
        set("id", json!(self.id));
        set("type", json!(self.shape.kind()));
        set("x", json!(self.x));
        set("y", json!(self.y));
        set("width", json!(frame.width()));
        set("height", json!(frame.height()));
        set("angle", json!(self.angle));
        set("strokeColor", json!(self.color));
        set("strokeStyle", json!(self.dash.name()));
        set("strokeWidth", json!(self.stroke));
        match &self.fill {
            Some(fill) => {
                set("backgroundColor", json!(fill.color));
                set("fillStyle", json!(fill.style.name()));
            }
            None => {
                set("backgroundColor", json!("transparent"));
                set("fillStyle", json!("solid"));
            }
        }
        set("groupIds", json!(self.groups));
        match &self.shape {
            Shape::Line { points } | Shape::Arrow { points } | Shape::Draw { points } => {
                let points: Vec<Value> = points.iter().map(|p| json!([p.x, p.y])).collect();
                set("points", Value::Array(points));
            }
            Shape::Text { text, size } => {
                set("text", json!(text));
                set("originalText", json!(text));
                set("fontSize", json!(size));
                set("containerId", json!(self.container));
            }
            Shape::Image { file, .. } => set("fileId", json!(file)),
            Shape::Frame { name, .. } => set("name", json!(name)),
            _ => {}
        }
        Value::Object(raw)
    }

    /// Sets a field leaf has no typed place for (roughness, arrowheads…).
    pub fn set_field(&mut self, key: &str, value: Value) {
        self.raw.insert(key.to_owned(), value);
    }

    /// Where a line from `from` toward this shape's middle first meets its
    /// outline, backed off by `gap`.
    pub fn edge(&self, from: Pt, gap: f64) -> Option<Pt> {
        let to = self.center();
        let length = from.distance(to);
        if length == 0.0 {
            return None;
        }
        let mut nearest: Option<(f64, Pt)> = None;
        for (points, _) in self.paths() {
            for pair in points.windows(2) {
                let Some(hit) = crossing(from, to, pair[0], pair[1]) else {
                    continue;
                };
                let d = from.distance(hit);
                if nearest.is_none_or(|(best, _)| d < best) {
                    nearest = Some((d, hit));
                }
            }
        }
        let (d, hit) = nearest?;
        let back = (gap / d.max(f64::EPSILON)).min(1.0);
        Some(Pt::new(
            hit.x + (from.x - hit.x) * back,
            hit.y + (from.y - hit.y) * back,
        ))
    }

    /// A field as read from the file, for what leaf draws but doesn't edit.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.raw.get(key).filter(|v| !v.is_null())
    }

    pub fn opacity(&self) -> f64 {
        self.field("opacity")
            .and_then(Value::as_f64)
            .map_or(1.0, |o| (o / 100.0).clamp(0.0, 1.0))
    }

    /// 0 is clean lines; 1 and 2 are Excalidraw's hand-drawn wobble.
    pub fn roughness(&self) -> f64 {
        self.field("roughness")
            .and_then(Value::as_f64)
            .unwrap_or(1.0)
    }

    pub fn seed(&self) -> u64 {
        self.field("seed").and_then(Value::as_u64).unwrap_or(1)
    }

    pub fn rounded(&self) -> bool {
        self.field("roundness").is_some()
    }

    /// Excalidraw's font: 1 hand-drawn, 2 normal, 3 code.
    pub fn font(&self) -> i64 {
        self.field("fontFamily")
            .and_then(Value::as_i64)
            .unwrap_or(1)
    }

    pub fn align(&self) -> &str {
        self.field("textAlign")
            .and_then(Value::as_str)
            .unwrap_or("left")
    }

    /// The head at `end` of an arrow or line.
    pub fn head(&self, end: End) -> Option<Head> {
        let key = end.key("Arrowhead");
        match (self.raw.get(&key), &self.shape, end) {
            (Some(Value::String(name)), _, _) => Some(Head::parse(name)),
            (None, Shape::Arrow { .. }, End::End) => Some(Head::Arrow),
            _ => None,
        }
    }

    /// The element an arrow's `end` is attached to.
    pub fn binding(&self, end: End) -> Option<&str> {
        self.field(&end.key("Binding"))?.get("elementId")?.as_str()
    }

    pub fn bind(&mut self, end: End, to: Option<&str>) {
        let value = to.map_or(
            Value::Null,
            |id| json!({ "elementId": id, "focus": 0, "gap": 4 }),
        );
        self.raw.insert(end.key("Binding"), value);
    }

    /// Notes that element `id` (an arrow, or text) is attached to this one.
    pub fn add_bound(&mut self, id: &str, kind: &str) {
        let mut bound = self.bound();
        if !bound.iter().any(|(b, _)| b == id) {
            bound.push((id.to_owned(), kind.to_owned()));
        }
        self.set_bound(bound);
    }

    /// Forgets attached elements that `keep` turns down.
    pub fn retain_bound(&mut self, keep: impl Fn(&str) -> bool) {
        let bound = self.bound();
        if bound.iter().all(|(id, _)| keep(id)) {
            return;
        }
        self.set_bound(bound.into_iter().filter(|(id, _)| keep(id)).collect());
    }

    fn bound(&self) -> Vec<(String, String)> {
        self.field("boundElements")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|b| {
                        Some((
                            b.get("id")?.as_str()?.to_owned(),
                            b.get("type")?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set_bound(&mut self, bound: Vec<(String, String)>) {
        let value = if bound.is_empty() {
            Value::Null
        } else {
            Value::Array(
                bound
                    .into_iter()
                    .map(|(id, kind)| json!({ "id": id, "type": kind }))
                    .collect(),
            )
        };
        self.raw.insert("boundElements".into(), value);
    }

    /// The element as it is before turning.
    pub fn frame(&self) -> Frame {
        let (x, y) = (self.x, self.y);
        let sized = |w: f64, h: f64| Frame {
            x0: x,
            y0: y,
            x1: x + w,
            y1: y + h,
        };
        match &self.shape {
            Shape::Rectangle { w, h }
            | Shape::Ellipse { w, h }
            | Shape::Diamond { w, h }
            | Shape::Image { w, h, .. }
            | Shape::Frame { w, h, .. }
            | Shape::Other { w, h, .. } => sized(*w, *h),
            Shape::Text { text, size } => {
                let (w, h) = text_size(text, *size);
                sized(w, h)
            }
            Shape::Line { points } | Shape::Arrow { points } | Shape::Draw { points } => {
                if points.is_empty() {
                    return sized(0.0, 0.0);
                }
                let f = extent(points);
                Frame {
                    x0: x + f.x0,
                    y0: y + f.y0,
                    x1: x + f.x1,
                    y1: y + f.y1,
                }
            }
        }
    }

    pub fn center(&self) -> Pt {
        self.frame().center()
    }

    /// The frame's corners after turning, clockwise from the top-left.
    pub fn corners(&self) -> [Pt; 4] {
        let f = self.frame();
        let c = f.center();
        [
            Pt::new(f.x0, f.y0),
            Pt::new(f.x1, f.y0),
            Pt::new(f.x1, f.y1),
            Pt::new(f.x0, f.y1),
        ]
        .map(|p| p.turn(c, self.angle))
    }

    /// The upright rectangle around the element as it's drawn.
    pub fn bounds(&self) -> Frame {
        if self.angle == 0.0 {
            return self.frame();
        }
        extent(&self.corners())
    }

    /// The element's outline as polylines before turning, and whether
    /// each is closed.
    fn local_paths(&self) -> Vec<(Vec<Pt>, bool)> {
        let at = |dx: f64, dy: f64| Pt::new(self.x + dx, self.y + dy);
        match &self.shape {
            Shape::Rectangle { w, h }
            | Shape::Image { w, h, .. }
            | Shape::Frame { w, h, .. }
            | Shape::Other { w, h, .. } => vec![(
                vec![
                    at(0.0, 0.0),
                    at(*w, 0.0),
                    at(*w, *h),
                    at(0.0, *h),
                    at(0.0, 0.0),
                ],
                true,
            )],
            Shape::Diamond { w, h } => vec![(
                vec![
                    at(w / 2.0, 0.0),
                    at(*w, h / 2.0),
                    at(w / 2.0, *h),
                    at(0.0, h / 2.0),
                    at(w / 2.0, 0.0),
                ],
                true,
            )],
            Shape::Ellipse { w, h } => {
                let (rx, ry) = (w / 2.0, h / 2.0);
                vec![(
                    (0..=48)
                        .map(|i| {
                            let a = f64::from(i) / 48.0 * std::f64::consts::TAU;
                            at(rx + rx * a.cos(), ry + ry * a.sin())
                        })
                        .collect(),
                    true,
                )]
            }
            Shape::Line { points } | Shape::Arrow { points } | Shape::Draw { points } => {
                vec![(points.iter().map(|p| at(p.x, p.y)).collect(), false)]
            }
            Shape::Text { .. } => Vec::new(),
        }
    }

    /// The outline as drawn: polylines on the board, and whether each is closed.
    pub fn paths(&self) -> Vec<(Vec<Pt>, bool)> {
        let c = self.center();
        self.local_paths()
            .into_iter()
            .map(|(points, closed)| {
                (
                    points.into_iter().map(|p| p.turn(c, self.angle)).collect(),
                    closed,
                )
            })
            .collect()
    }

    /// The heads on an arrow or line, as drawn.
    pub fn tips(&self) -> Vec<Tip> {
        let (Shape::Arrow { points } | Shape::Line { points }) = &self.shape else {
            return Vec::new();
        };
        let c = self.center();
        let world: Vec<Pt> = points
            .iter()
            .map(|p| Pt::new(self.x + p.x, self.y + p.y).turn(c, self.angle))
            .collect();
        let mut tips = Vec::new();
        for (end, tip, from) in [
            (End::End, world.last(), world.iter().rev().nth(1)),
            (End::Start, world.first(), world.get(1)),
        ] {
            let (Some(head), Some(&tip), Some(&from)) = (self.head(end), tip, from) else {
                continue;
            };
            let length = tip.distance(from);
            if length == 0.0 {
                continue;
            }
            tips.push(Tip {
                head,
                at: tip,
                angle: (tip.y - from.y).atan2(tip.x - from.x),
                size: (12.0 + self.stroke * 3.0).min(length / 2.0),
            });
        }
        tips
    }

    /// The straight strokes that draw the element, heads included. Text
    /// has none; it's printed.
    pub fn strokes(&self) -> Vec<(Pt, Pt)> {
        let mut paths: Vec<Vec<Pt>> = self.paths().into_iter().map(|(p, _)| p).collect();
        for tip in self.tips() {
            paths.extend(tip.paths().0);
        }
        paths
            .iter()
            .flat_map(|points| points.windows(2).map(|pair| (pair[0], pair[1])))
            .collect()
    }

    /// Whether `p` touches the element, within `slack` pixels. Shapes and
    /// text are hit anywhere inside; lines only near their strokes.
    pub fn hit(&self, p: Pt, slack: f64) -> bool {
        let local = p.turn(self.center(), -self.angle);
        match &self.shape {
            Shape::Line { .. } | Shape::Arrow { .. } | Shape::Draw { .. } => {
                let paths = self.local_paths();
                paths.iter().any(|(points, _)| match points.as_slice() {
                    [only] => local.distance(*only) <= slack,
                    points => points
                        .windows(2)
                        .any(|pair| distance_to_segment(local, pair[0], pair[1]) <= slack),
                })
            }
            // A frame is picked by its edge or its name, so what's in it can be picked.
            Shape::Frame { .. } => {
                let f = self.frame();
                let on_name =
                    local.x >= f.x0 && local.x <= f.x1 && (f.y0 - 24.0..=f.y0).contains(&local.y);
                on_name
                    || self.local_paths().iter().any(|(points, _)| {
                        points
                            .windows(2)
                            .any(|pair| distance_to_segment(local, pair[0], pair[1]) <= slack)
                    })
            }
            Shape::Diamond { .. } | Shape::Ellipse { .. } => {
                self.local_paths().iter().any(|(points, _)| {
                    inside(local, points)
                        || points
                            .windows(2)
                            .any(|pair| distance_to_segment(local, pair[0], pair[1]) <= slack)
                })
            }
            _ => {
                let f = self.frame().grow(slack);
                local.x >= f.x0 && local.x <= f.x1 && local.y >= f.y0 && local.y <= f.y1
            }
        }
    }

    pub fn shift(&mut self, dx: f64, dy: f64) {
        self.x += dx;
        self.y += dy;
    }

    /// Stretches the element's upright frame from `from` onto `to`.
    pub fn stretch(&mut self, from: Frame, to: Frame) {
        let origin = from.map(Pt::new(self.x, self.y), to);
        let (sx, sy) = (
            if from.width() > 0.0 {
                to.width() / from.width()
            } else {
                1.0
            },
            if from.height() > 0.0 {
                to.height() / from.height()
            } else {
                1.0
            },
        );
        (self.x, self.y) = (origin.x, origin.y);
        match &mut self.shape {
            Shape::Rectangle { w, h }
            | Shape::Ellipse { w, h }
            | Shape::Diamond { w, h }
            | Shape::Image { w, h, .. }
            | Shape::Frame { w, h, .. }
            | Shape::Other { w, h, .. } => {
                *w *= sx;
                *h *= sy;
            }
            Shape::Line { points } | Shape::Arrow { points } | Shape::Draw { points } => {
                for p in points {
                    (p.x, p.y) = (p.x * sx, p.y * sy);
                }
            }
            Shape::Text { size, .. } => *size = (*size * sy).max(4.0),
        }
    }

    /// Writes the points an arrow or line runs through, from the first.
    pub fn set_path(&mut self, world: &[Pt]) {
        let (Shape::Arrow { points } | Shape::Line { points }) = &mut self.shape else {
            return;
        };
        let Some(&first) = world.first() else {
            return;
        };
        (self.x, self.y) = (first.x, first.y);
        *points = world.iter().map(|p| p.minus(first)).collect();
    }

    /// The points an arrow or line runs through, on the board, before turning.
    pub fn path(&self) -> Vec<Pt> {
        match &self.shape {
            Shape::Arrow { points } | Shape::Line { points } => points
                .iter()
                .map(|p| Pt::new(self.x + p.x, self.y + p.y))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// One line naming the element, for the agent.
    pub fn describe(&self) -> String {
        let f = self.bounds();
        let mut out = format!(
            "{} {} at ({:.0}, {:.0}), {:.0}×{:.0}",
            self.shape.kind(),
            self.id,
            f.x0,
            f.y0,
            f.width(),
            f.height()
        );
        if self.color != INK {
            out.push_str(&format!(", {}", self.color));
        }
        if let Some(fill) = &self.fill {
            out.push_str(&format!(", filled {} {}", fill.style.name(), fill.color));
        }
        if self.angle != 0.0 {
            out.push_str(&format!(", turned {:.0}°", self.angle.to_degrees()));
        }
        match &self.shape {
            Shape::Text { text, .. } => out.push_str(&format!(": {text:?}")),
            Shape::Frame { name, .. } if !name.is_empty() => {
                out.push_str(&format!(": {name:?}"));
            }
            _ => {}
        }
        for end in [End::Start, End::End] {
            if let Some(to) = self.binding(end) {
                let which = if end == End::Start { "from" } else { "to" };
                out.push_str(&format!(", {which} {to}"));
            }
        }
        out
    }
}

fn random() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
}

pub fn new_id() -> String {
    format!("{:016x}", random())
}

/// A file name for a new board in `dir` that nothing there has yet, like
/// `board-misty-harbor.excalidraw`, created as an empty board to claim it.
pub fn fresh_path(dir: &Path) -> Result<PathBuf, BoardFailed> {
    const FIRST: &[&str] = &[
        "amber", "brisk", "calm", "dusky", "eager", "fern", "gentle", "hazy", "ivory", "jolly",
        "keen", "lucky", "misty", "nimble", "olive", "plum", "quiet", "rapid", "sunny", "tidy",
        "umber", "vivid", "windy", "young", "zesty",
    ];
    const SECOND: &[&str] = &[
        "atlas", "brook", "cedar", "delta", "ember", "field", "grove", "harbor", "island",
        "jungle", "kite", "lagoon", "meadow", "nebula", "orbit", "pebble", "quarry", "river",
        "summit", "tundra", "valley", "willow", "yarrow", "zephyr",
    ];
    let mut attempt = 0;
    loop {
        attempt += 1;
        let n = random() as usize;
        let words = format!(
            "{}-{}",
            FIRST[n % FIRST.len()],
            SECOND[(n / FIRST.len()) % SECOND.len()]
        );
        // After many taken names, a number keeps the name free.
        let name = if attempt < 32 {
            format!("board-{words}.excalidraw")
        } else {
            format!("board-{words}-{:04x}.excalidraw", (n >> 16) & 0xffff)
        };
        let path = dir.join(name);
        // Creating the file claims the name, so two boards started at once
        // can't both pick it and save over each other.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                return match file.write_all(Board::default().to_json().as_bytes()) {
                    Ok(()) => Ok(path),
                    Err(cause) => {
                        // A half-written board would hold the name forever.
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        Err(BoardFailed::Create {
                            path: path.display().to_string(),
                            cause,
                        })
                    }
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(cause) => {
                return Err(BoardFailed::Create {
                    path: path.display().to_string(),
                    cause,
                });
            }
        }
    }
}

/// Which way `reorder` moves elements in the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Forward,
    Backward,
    Front,
    Back,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Board {
    /// Bottom to top.
    pub elements: Vec<Element>,
    /// The rest of the file (appState, files…), kept for the next save.
    raw: Map<String, Value>,
}

impl Board {
    pub fn parse(text: &str) -> Result<Board, String> {
        let Value::Object(mut raw) =
            serde_json::from_str::<Value>(text).map_err(|e| e.to_string())?
        else {
            return Err("the top level isn't a JSON object".into());
        };
        // Agents sometimes write only the elements, so a missing type is
        // fine as long as there are elements.
        match raw.get("type") {
            Some(Value::String(kind)) if kind == "excalidraw" || kind == "excalidraw/clipboard" => {
            }
            None if raw.contains_key("elements") => {}
            None => {
                return Err(
                    "it has neither \"type\": \"excalidraw\" nor \"elements\", so it isn't an Excalidraw board".into(),
                );
            }
            Some(kind) => return Err(format!("its \"type\" is {kind}, not \"excalidraw\"")),
        }
        let elements = match raw.remove("elements") {
            Some(Value::Array(elements)) => {
                let mut kept = Vec::with_capacity(elements.len());
                for value in elements {
                    kept.extend(Element::from_value(value)?);
                }
                kept
            }
            None => Vec::new(),
            Some(_) => return Err("\"elements\" isn't a list".into()),
        };
        Ok(Board { elements, raw })
    }

    /// The board at `path`; empty when there's no file yet.
    pub fn load(path: &Path) -> Result<Board, BoardFailed> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Board::default()),
            Ok(text) => Board::parse(&text).map_err(|cause| BoardFailed::Parse {
                path: path.display().to_string(),
                cause,
            }),
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => Ok(Board::default()),
            Err(cause) => Err(BoardFailed::Read {
                path: path.display().to_string(),
                cause,
            }),
        }
    }

    pub fn to_json(&self) -> String {
        let mut out = Map::new();
        out.insert("type".into(), json!("excalidraw"));
        out.insert("version".into(), json!(2));
        out.insert("source".into(), json!("leaf"));
        out.extend(self.raw.clone());
        out.insert(
            "elements".into(),
            Value::Array(self.elements.iter().map(Element::to_value).collect()),
        );
        out.entry("appState")
            .or_insert_with(|| json!({ "viewBackgroundColor": "#ffffff", "gridSize": null }));
        out.entry("files").or_insert_with(|| json!({}));
        serde_json::to_string_pretty(&Value::Object(out)).unwrap_or_default()
    }

    /// Writes the board beside `path` and moves it into place, so a reader
    /// never sees half a file.
    pub fn save(&self, path: &Path) -> Result<(), BoardFailed> {
        let failed = |cause| BoardFailed::Write {
            path: path.display().to_string(),
            cause,
        };
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let staging = path.with_file_name(format!(".{name}.leaf-saving"));
        std::fs::write(&staging, self.to_json()).map_err(failed)?;
        // The new file takes the old one's permissions, so a private board stays private.
        if let Ok(meta) = std::fs::metadata(path) {
            std::fs::set_permissions(&staging, meta.permissions()).map_err(failed)?;
        }
        std::fs::rename(&staging, path).map_err(failed)
    }

    /// Keeps a picture's bytes in the file and returns its file id, the
    /// same for the same bytes.
    pub fn add_picture(&mut self, mime: &str, bytes: &[u8]) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let id: String = Sha256::digest(bytes)
            .iter()
            .take(10)
            .map(|b| format!("{b:02x}"))
            .collect();
        let url = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let files = self
            .raw
            .entry("files")
            .or_insert_with(|| Value::Object(Map::new()));
        if !files.is_object() {
            *files = Value::Object(Map::new());
        }
        if let Value::Object(files) = files {
            files.insert(
                id.clone(),
                json!({ "mimeType": mime, "id": id, "dataURL": url, "created": created }),
            );
        }
        id
    }

    /// A picture's pixels as a data URL, by file id.
    pub fn picture(&self, file: &str) -> Option<&str> {
        self.raw.get("files")?.get(file)?.get("dataURL")?.as_str()
    }

    /// The topmost element `p` touches. Text written inside a shape picks
    /// the shape.
    pub fn hit(&self, p: Pt, slack: f64) -> Option<usize> {
        let index = self.elements.iter().rposition(|e| e.hit(p, slack))?;
        Some(
            self.elements[index]
                .container
                .as_deref()
                .and_then(|id| self.find(id))
                .unwrap_or(index),
        )
    }

    /// The topmost shape an arrow can attach to at `p`, leaving out `except`.
    pub fn holder(&self, p: Pt, slack: f64, except: &str) -> Option<usize> {
        self.elements
            .iter()
            .rposition(|e| e.id != except && e.shape.holds() && e.hit(p, slack))
    }

    pub fn find(&self, id: &str) -> Option<usize> {
        self.elements.iter().position(|e| e.id == id)
    }

    pub fn get(&self, id: &str) -> Option<&Element> {
        self.elements.iter().find(|e| e.id == id)
    }

    /// The upright rectangle around every element.
    pub fn bounds(&self) -> Option<Frame> {
        self.elements
            .iter()
            .map(Element::bounds)
            .reduce(Frame::union)
    }

    /// `ids` with every element grouped with them and the text inside them.
    pub fn closure(&self, ids: &[String]) -> Vec<String> {
        let mut out: Vec<String> = ids.to_vec();
        let outer: Vec<&str> = ids
            .iter()
            .filter_map(|id| self.get(id)?.groups.last().map(String::as_str))
            .collect();
        for element in &self.elements {
            let grouped = element
                .groups
                .last()
                .is_some_and(|g| outer.contains(&g.as_str()));
            if grouped && !out.contains(&element.id) {
                out.push(element.id.clone());
            }
        }
        let frames: Vec<(String, Frame)> = self
            .elements
            .iter()
            .filter(|e| matches!(e.shape, Shape::Frame { .. }) && out.contains(&e.id))
            .map(|e| (e.id.clone(), e.frame()))
            .collect();
        for element in &self.elements {
            let framed = frames.iter().any(|(id, frame)| {
                *id != element.id
                    && (element.field("frameId").and_then(Value::as_str) == Some(id.as_str())
                        || frame.contains(element.bounds()))
            });
            if framed && !out.contains(&element.id) {
                out.push(element.id.clone());
            }
        }
        for element in &self.elements {
            let held = element.container.as_ref().is_some_and(|c| out.contains(c));
            if held && !out.contains(&element.id) {
                out.push(element.id.clone());
            }
        }
        out
    }

    /// Puts `ids` in a new group, around any groups they're already in.
    pub fn group(&mut self, ids: &[String]) {
        let group = new_id();
        for element in &mut self.elements {
            if ids.contains(&element.id) {
                element.groups.push(group.clone());
            }
        }
    }

    /// Takes `ids` out of their outermost group.
    pub fn ungroup(&mut self, ids: &[String]) {
        for element in &mut self.elements {
            if ids.contains(&element.id) {
                element.groups.pop();
            }
        }
    }

    pub fn reorder(&mut self, ids: &[String], order: Order) {
        let picked = |e: &Element| ids.contains(&e.id);
        match order {
            Order::Front | Order::Back => {
                let (mut moved, rest): (Vec<Element>, Vec<Element>) =
                    std::mem::take(&mut self.elements)
                        .into_iter()
                        .partition(picked);
                self.elements = if order == Order::Front {
                    let mut all = rest;
                    all.append(&mut moved);
                    all
                } else {
                    moved.extend(rest);
                    moved
                };
            }
            Order::Forward => {
                for i in (0..self.elements.len().saturating_sub(1)).rev() {
                    if picked(&self.elements[i]) && !picked(&self.elements[i + 1]) {
                        self.elements.swap(i, i + 1);
                    }
                }
            }
            Order::Backward => {
                for i in 1..self.elements.len() {
                    if picked(&self.elements[i]) && !picked(&self.elements[i - 1]) {
                        self.elements.swap(i, i - 1);
                    }
                }
            }
        }
    }

    /// Copies `ids` a little down and right of where they are, with their
    /// groups, text and arrows between them kept among the copies. Returns
    /// the copies' ids.
    pub fn duplicate(&mut self, ids: &[String]) -> Vec<String> {
        let renamed: std::collections::HashMap<String, String> =
            ids.iter().map(|id| (id.clone(), new_id())).collect();
        let mut groups: std::collections::HashMap<String, String> = Default::default();
        let mut copies = Vec::new();
        for element in &self.elements {
            let Some(id) = renamed.get(&element.id) else {
                continue;
            };
            let mut copy = element.clone();
            copy.id = id.clone();
            copy.shift(10.0, 10.0);
            copy.groups = element
                .groups
                .iter()
                .map(|g| groups.entry(g.clone()).or_insert_with(new_id).clone())
                .collect();
            copy.container = element
                .container
                .as_ref()
                .and_then(|c| renamed.get(c).cloned());
            for end in [End::Start, End::End] {
                let to = copy.binding(end).and_then(|b| renamed.get(b).cloned());
                copy.bind(end, to.as_deref());
            }
            copy.retain_bound(|_| false);
            copy.raw
                .insert("seed".into(), json!(random() % 2_000_000_000));
            copies.push(copy);
        }
        let ids: Vec<String> = copies.iter().map(|c| c.id.clone()).collect();
        self.elements.extend(copies);
        self.relink();
        ids
    }

    /// Drops links to elements that are gone, and rebuilds each shape's list
    /// of what's attached to it from the arrows and text that point at it.
    pub fn relink(&mut self) {
        let ids: Vec<String> = self.elements.iter().map(|e| e.id.clone()).collect();
        let mut attached: Vec<(String, String, String)> = Vec::new();
        for element in &mut self.elements {
            for end in [End::Start, End::End] {
                if let Some(to) = element.binding(end).map(str::to_owned) {
                    if ids.contains(&to) {
                        attached.push((to, element.id.clone(), "arrow".into()));
                    } else {
                        element.bind(end, None);
                    }
                }
            }
            if let Some(c) = element.container.clone() {
                if ids.contains(&c) {
                    attached.push((c, element.id.clone(), "text".into()));
                } else {
                    element.container = None;
                }
            }
        }
        for element in &mut self.elements {
            let mine: Vec<&(String, String, String)> = attached
                .iter()
                .filter(|(to, ..)| *to == element.id)
                .collect();
            element.retain_bound(|id| mine.iter().any(|(_, from, _)| from == id));
            for (_, from, kind) in mine {
                element.add_bound(from, kind);
            }
        }
    }

    /// Moves the ends of arrows attached to shapes that moved, changed size
    /// or turned since `before`, so arrows stay attached. Arrows in `moved`
    /// were moved themselves and are left where they were put.
    pub fn follow(&mut self, before: &Board, moved: &[String]) {
        // Each changed shape's frame and turn, before and now.
        let changes: Vec<_> = self
            .elements
            .iter()
            .filter_map(|e| {
                let old = before.get(&e.id)?;
                let (was, now) = ((old.frame(), old.angle), (e.frame(), e.angle));
                (was != now).then(|| (e.id.clone(), was, now))
            })
            .collect();
        for element in &mut self.elements {
            if moved.contains(&element.id) {
                continue;
            }
            let Some(old) = before.get(&element.id) else {
                continue;
            };
            // The arrow's points where they're drawn, so a turned arrow
            // keeps its look once its turn is written into its points.
            let middle = old.center();
            let mut path: Vec<Pt> = old
                .path()
                .into_iter()
                .map(|p| p.turn(middle, old.angle))
                .collect();
            let mut changed = false;
            for (end, index) in [(End::Start, 0), (End::End, path.len().wrapping_sub(1))] {
                let Some((_, (was, was_angle), (now, now_angle))) = element
                    .binding(end)
                    .and_then(|to| changes.iter().find(|(id, ..)| id == to))
                else {
                    continue;
                };
                if let Some(p) = path.get_mut(index) {
                    let upright = p.turn(was.center(), -was_angle);
                    *p = was.map(upright, *now).turn(now.center(), *now_angle);
                    changed = true;
                }
            }
            if changed {
                element.set_path(&path);
                element.angle = 0.0;
            }
        }
    }

    /// Centers the text written inside `container` in it.
    pub fn recenter(&mut self, container: &str) {
        let Some(holder) = self.get(container) else {
            return;
        };
        let (center, angle) = (holder.center(), holder.angle);
        for element in &mut self.elements {
            if element.container.as_deref() != Some(container) {
                continue;
            }
            let f = element.frame();
            element.x = center.x - f.width() / 2.0;
            element.y = center.y - f.height() / 2.0;
            element.angle = angle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r##"{
      "type": "excalidraw",
      "version": 2,
      "source": "https://excalidraw.com",
      "elements": [
        {"id": "box", "type": "rectangle", "x": 10, "y": 20, "width": 100, "height": 50,
         "strokeColor": "#e03131", "roughness": 2, "customData": {"keep": true},
         "backgroundColor": "#ffc9c9", "fillStyle": "cross-hatch", "strokeStyle": "dashed",
         "groupIds": ["g1"]},
        {"id": "gone", "type": "ellipse", "x": 0, "y": 0, "width": 5, "height": 5, "isDeleted": true},
        {"id": "go", "type": "arrow", "x": 200, "y": 100, "width": 50, "height": 30,
         "points": [[0, 0], [50, 30]], "startArrowhead": "dot", "endArrowhead": "triangle"},
        {"id": "hi", "type": "text", "x": 0, "y": 200, "text": "hi\nthere", "fontSize": 20},
        {"id": "pic", "type": "image", "x": 5, "y": 5, "width": 40, "height": 40, "fileId": "f1"},
        {"id": "embed", "type": "embeddable", "x": 300, "y": 300, "width": 40, "height": 40}
      ],
      "appState": {"viewBackgroundColor": "#fafafa"},
      "files": {"f1": {"mimeType": "image/png", "dataURL": "data:image/png;base64,AAAA"}}
    }"##;

    fn board() -> Board {
        Board::parse(FILE).expect("parses")
    }

    fn rect(id: &str, x: f64, y: f64, w: f64, h: f64) -> Element {
        let mut e = Element::new(Shape::Rectangle { w, h }, Pt::new(x, y), INK);
        e.id = id.into();
        e
    }

    #[test]
    fn reading_skips_deleted_elements_and_keeps_what_it_cannot_draw() {
        let board = board();
        let ids: Vec<&str> = board.elements.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["box", "go", "hi", "pic", "embed"]);
        assert_eq!(
            board.elements[3].shape,
            Shape::Image {
                w: 40.0,
                h: 40.0,
                file: "f1".into()
            }
        );
        assert_eq!(board.picture("f1"), Some("data:image/png;base64,AAAA"));
        assert!(
            matches!(&board.elements[4].shape, Shape::Other { kind, .. } if kind == "embeddable")
        );
    }

    #[test]
    fn styles_are_read_from_excalidraw_fields() {
        let board = board();
        let shape = &board.elements[0];
        assert_eq!(
            shape.fill,
            Some(Fill {
                color: "#ffc9c9".into(),
                style: FillStyle::CrossHatch
            })
        );
        assert_eq!(shape.dash, Dash::Dashed);
        assert_eq!(shape.groups, ["g1"]);
        let arrow = &board.elements[1];
        assert_eq!(arrow.head(End::Start), Some(Head::Dot));
        assert_eq!(arrow.head(End::End), Some(Head::Triangle));
    }

    #[test]
    fn saving_keeps_fields_and_file_parts_leaf_does_not_use() {
        let mut board = board();
        board.elements[0].shift(5.0, 5.0);
        let saved: Value = serde_json::from_str(&board.to_json()).expect("valid JSON");
        let first = &saved["elements"][0];
        assert_eq!(first["x"], json!(15.0));
        assert_eq!(first["roughness"], json!(2));
        assert_eq!(first["customData"], json!({"keep": true}));
        assert_eq!(first["fillStyle"], json!("cross-hatch"));
        assert_eq!(saved["elements"][3]["fileId"], json!("f1"));
        assert_eq!(saved["appState"]["viewBackgroundColor"], json!("#fafafa"));
        assert_eq!(saved["files"]["f1"]["mimeType"], json!("image/png"));
        let json = board.to_json();
        assert_eq!(Board::parse(&json).expect("reparses").to_json(), json);
    }

    #[test]
    fn a_new_element_has_what_excalidraw_needs_to_open_it() {
        let mut board = Board::default();
        board.elements.push(Element::new(
            Shape::Arrow {
                points: vec![Pt::new(0.0, 0.0), Pt::new(-40.0, 20.0)],
            },
            Pt::new(100.0, 100.0),
            INK,
        ));
        let saved: Value = serde_json::from_str(&board.to_json()).expect("valid JSON");
        assert_eq!(saved["type"], json!("excalidraw"));
        let arrow = &saved["elements"][0];
        for key in [
            "id",
            "type",
            "x",
            "y",
            "width",
            "height",
            "angle",
            "strokeColor",
            "backgroundColor",
            "fillStyle",
            "strokeWidth",
            "strokeStyle",
            "roughness",
            "opacity",
            "seed",
            "version",
            "versionNonce",
            "isDeleted",
            "groupIds",
            "points",
        ] {
            assert!(arrow.get(key).is_some(), "{key} is missing");
        }
        assert_eq!(arrow["endArrowhead"], json!("arrow"));
        assert_eq!(arrow["width"], json!(40.0));
        assert_eq!(arrow["height"], json!(20.0));
    }

    #[test]
    fn lines_are_hit_near_their_strokes_and_shapes_inside_their_outline() {
        let board = board();
        let arrow = &board.elements[1];
        assert!(arrow.hit(Pt::new(225.0, 115.0), 3.0));
        assert!(!arrow.hit(Pt::new(240.0, 100.0), 3.0));
        assert!(board.elements[0].hit(Pt::new(60.0, 40.0), 0.0));
        // The picture sits over the box's corner, so it wins there.
        assert_eq!(board.hit(Pt::new(20.0, 30.0), 0.0), Some(3));
        assert_eq!(board.hit(Pt::new(500.0, 500.0), 0.0), None);
        let diamond = Element::new(
            Shape::Diamond { w: 100.0, h: 100.0 },
            Pt::new(0.0, 0.0),
            INK,
        );
        assert!(diamond.hit(Pt::new(50.0, 50.0), 0.0));
        assert!(
            !diamond.hit(Pt::new(5.0, 5.0), 0.0),
            "the corner is outside"
        );
    }

    #[test]
    fn a_turned_shape_is_hit_and_bounded_where_it_is_drawn() {
        let mut bar = rect("bar", 0.0, 45.0, 100.0, 10.0);
        bar.angle = std::f64::consts::FRAC_PI_2;
        assert!(bar.hit(Pt::new(50.0, 10.0), 0.0));
        assert!(!bar.hit(Pt::new(10.0, 50.0), 0.0));
        let b = bar.bounds();
        assert!((b.x0 - 45.0).abs() < 1e-9 && (b.y0 - 0.0).abs() < 1e-9);
        assert!((b.width() - 10.0).abs() < 1e-9 && (b.height() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn text_is_measured_from_its_longest_line() {
        let board = board();
        let f = board.elements[2].frame();
        assert_eq!((f.x0, f.y0, f.x1, f.y1), (0.0, 200.0, 60.0, 250.0));
    }

    #[test]
    fn stretching_scales_points_and_text() {
        let mut arrow = Element::new(
            Shape::Arrow {
                points: vec![Pt::new(0.0, 0.0), Pt::new(100.0, 50.0)],
            },
            Pt::new(10.0, 10.0),
            INK,
        );
        let from = arrow.frame();
        arrow.stretch(
            from,
            Frame {
                x0: 0.0,
                y0: 0.0,
                x1: 200.0,
                y1: 25.0,
            },
        );
        assert_eq!(arrow.path(), [Pt::new(0.0, 0.0), Pt::new(200.0, 25.0)]);
    }

    #[test]
    fn arrows_follow_the_shapes_they_are_attached_to() {
        let mut board = Board::default();
        board.elements.push(rect("a", 0.0, 0.0, 100.0, 100.0));
        board.elements.push(rect("b", 300.0, 0.0, 100.0, 100.0));
        let mut arrow = Element::new(
            Shape::Arrow {
                points: vec![Pt::new(0.0, 0.0), Pt::new(196.0, 0.0)],
            },
            Pt::new(104.0, 50.0),
            INK,
        );
        arrow.bind(End::Start, Some("a"));
        arrow.bind(End::End, Some("b"));
        board.elements.push(arrow);
        board.relink();
        let before = board.clone();
        board.elements[1].shift(0.0, 50.0);
        board.follow(&before, &["b".into()]);
        // Following again from the same start doesn't add up.
        board.elements[1].shift(0.0, 50.0);
        board.follow(&before, &["b".into()]);
        assert_eq!(
            board.elements[2].path(),
            [Pt::new(104.0, 50.0), Pt::new(300.0, 150.0)]
        );
        let saved: Value = serde_json::from_str(&board.to_json()).expect("valid JSON");
        assert_eq!(
            saved["elements"][1]["boundElements"][0]["id"],
            json!(board.elements[2].id)
        );
    }

    #[test]
    fn grouped_elements_and_text_inside_shapes_go_together() {
        let mut board = Board::default();
        board.elements.push(rect("a", 0.0, 0.0, 10.0, 10.0));
        board.elements.push(rect("b", 20.0, 0.0, 10.0, 10.0));
        let mut label = Element::new(
            Shape::Text {
                text: "hi".into(),
                size: 20.0,
            },
            Pt::new(0.0, 0.0),
            INK,
        );
        label.container = Some("b".into());
        board.elements.push(label);
        board.group(&["a".into(), "b".into()]);
        let closure = board.closure(&["a".into()]);
        assert_eq!(closure.len(), 3);
        board.ungroup(&["a".into(), "b".into()]);
        assert_eq!(board.closure(&["a".into()]), ["a"]);
        // Clicking the label picks the box it's in.
        assert_eq!(board.hit(Pt::new(25.0, 5.0), 0.0), Some(1));
    }

    #[test]
    fn duplicates_get_new_ids_and_keep_their_links_among_themselves() {
        let mut board = Board::default();
        board.elements.push(rect("a", 0.0, 0.0, 10.0, 10.0));
        let mut label = Element::new(
            Shape::Text {
                text: "hi".into(),
                size: 20.0,
            },
            Pt::new(0.0, 0.0),
            INK,
        );
        label.container = Some("a".into());
        let label_id = label.id.clone();
        board.elements.push(label);
        let copies = board.duplicate(&["a".into(), label_id]);
        assert_eq!(board.elements.len(), 4);
        assert_eq!(
            board.elements[3].container.as_deref(),
            Some(copies[0].as_str())
        );
        assert_eq!(board.elements[2].x, 10.0);
    }

    #[test]
    fn reordering_moves_elements_through_the_stack() {
        let mut board = Board::default();
        for id in ["a", "b", "c"] {
            board.elements.push(rect(id, 0.0, 0.0, 1.0, 1.0));
        }
        let order = |b: &Board| b.elements.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
        board.reorder(&["a".into()], Order::Forward);
        assert_eq!(order(&board), ["b", "a", "c"]);
        board.reorder(&["a".into()], Order::Front);
        assert_eq!(order(&board), ["b", "c", "a"]);
        board.reorder(&["a".into()], Order::Back);
        assert_eq!(order(&board), ["a", "b", "c"]);
        board.reorder(&["c".into()], Order::Backward);
        assert_eq!(order(&board), ["a", "c", "b"]);
    }

    #[test]
    fn a_missing_file_is_an_empty_board_but_a_broken_one_is_refused() {
        let dir = std::env::temp_dir().join(format!("leaf-board-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let missing = dir.join("none.excalidraw");
        assert_eq!(Board::load(&missing).expect("empty"), Board::default());
        let broken = dir.join("broken.excalidraw");
        std::fs::write(&broken, "[1, 2]").expect("write");
        assert!(matches!(
            Board::load(&broken),
            Err(BoardFailed::Parse { .. })
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_board_gets_a_free_word_name() {
        let dir = std::env::temp_dir().join(format!("leaf-board-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let mut seen = Vec::new();
        for _ in 0..20 {
            let path = fresh_path(&dir).expect("a free name");
            let name = path.file_name().and_then(|n| n.to_str()).expect("a name");
            assert!(
                name.starts_with("board-") && name.ends_with(".excalidraw"),
                "{name}"
            );
            assert!(!seen.contains(&path), "{name}");
            // The name is claimed with an empty board that opens.
            assert!(Board::load(&path).expect("loads").elements.is_empty());
            seen.push(path);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_new_board_name_is_claimed_even_when_boards_start_together() {
        let dir = std::env::temp_dir().join(format!("leaf-board-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let dir = dir.clone();
                std::thread::spawn(move || fresh_path(&dir).expect("a free name"))
            })
            .collect();
        let mut paths: Vec<PathBuf> = threads
            .into_iter()
            .map(|t| t.join().expect("thread"))
            .collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 16);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_that_isnt_a_board_or_has_broken_elements_is_refused() {
        assert!(Board::parse(r#"{"name": "settings", "debug": true}"#).is_err());
        assert!(Board::parse(r#"{"type": "geojson", "elements": []}"#).is_err());
        assert!(Board::parse(r#"{"type": "excalidraw", "elements": [42]}"#).is_err());
        assert!(Board::parse(r#"{"type": "excalidraw", "elements": [{"id": "a"}]}"#).is_err());
        // A file an agent writes with only its elements is still a board.
        let board = Board::parse(r#"{"elements": [{"id": "a", "type": "rectangle"}]}"#)
            .expect("elements alone");
        assert_eq!(board.elements.len(), 1);
        assert!(Board::parse(r#"{"type": "excalidraw/clipboard", "elements": []}"#).is_ok());
    }

    #[test]
    fn arrows_follow_a_shape_that_turns() {
        let mut board = Board::default();
        board.elements.push(rect("a", 0.0, 0.0, 100.0, 100.0));
        let mut arrow = Element::new(
            Shape::Arrow {
                points: vec![Pt::new(0.0, 0.0), Pt::new(100.0, 0.0)],
            },
            Pt::new(100.0, 50.0),
            INK,
        );
        arrow.id = "go".into();
        arrow.bind(End::Start, Some("a"));
        board.elements.push(arrow);
        let before = board.clone();
        board.elements[0].angle = std::f64::consts::FRAC_PI_2;
        board.follow(&before, &["a".into()]);
        // The right edge's middle turns a quarter about the middle, to the bottom.
        let start = board.get("go").expect("arrow").path()[0];
        assert!(
            (start.x - 50.0).abs() < 1e-6 && (start.y - 100.0).abs() < 1e-6,
            "{start:?}"
        );
        assert_eq!(
            board.get("go").expect("arrow").path()[1],
            Pt::new(200.0, 50.0)
        );
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("leaf-board-mode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("private.excalidraw");
        Board::default().save(&path).expect("first save");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        board().save(&path).expect("second save");
        let mode = std::fs::metadata(&path)
            .expect("saved")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(dir);
    }
}
