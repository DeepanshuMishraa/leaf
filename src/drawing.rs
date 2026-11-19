//! DrawingML, the shapes, pictures and text boxes inside office files,
//! drawn as SVG: fills and gradients, outlines and arrowheads, preset and
//! custom geometry, and text with its inherited paragraph and run styles.
//! All lengths are points.

use std::f32::consts::PI;
use std::fmt::Write as _;
use std::rc::Rc;

use crate::ooxml::{Color, Local, Package, Rel, Theme, data_uri, emu, rel_by_id, roxmltree};
use crate::typeset::{self, Run, Style};
use roxmltree::Node;

/// An SVG being drawn.
#[derive(Default)]
pub struct Canvas {
    pub body: String,
    defs: String,
    next: usize,
}

impl Canvas {
    pub fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    pub fn def(&mut self, svg: &str) {
        self.defs.push_str(svg);
    }

    pub fn finish(self, width: f32, height: f32) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="{width:.2}" height="{height:.2}" viewBox="0 0 {width:.2} {height:.2}"><defs>{}</defs>{}</svg>"#,
            self.defs, self.body
        )
    }
}

/// A part of the package, for resolving the pictures it links to.
pub struct Part<'p> {
    pub package: &'p Package,
    pub rels: Vec<Rel>,
}

impl Part<'_> {
    /// A picture the part links to by relationship id, as a data URI.
    pub fn picture(&self, id: &str) -> Option<String> {
        let rel = rel_by_id(&self.rels, id).filter(|r| !r.external)?;
        data_uri(&self.package.bytes(&rel.target)?)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// How an area is filled.
#[derive(Debug, Clone)]
pub enum Paint {
    Solid(Color),
    Gradient {
        stops: Vec<(f32, Color)>,
        /// Degrees clockwise from left-to-right.
        angle: f32,
        radial: bool,
    },
    /// A picture stretched over the area.
    Picture(String),
}

impl Paint {
    /// A representative color, for places that take only one.
    pub fn color(&self) -> Option<Color> {
        match self {
            Paint::Solid(c) => Some(*c),
            Paint::Gradient { stops, .. } => stops.first().map(|(_, c)| *c),
            Paint::Picture(_) => None,
        }
    }
}

const FILLS: [&str; 6] = [
    "solidFill",
    "gradFill",
    "blipFill",
    "pattFill",
    "noFill",
    "grpFill",
];

/// The fill element among a shape's properties, if it names one.
pub fn fill_node<'a, 'i>(props: Node<'a, 'i>) -> Option<Node<'a, 'i>> {
    props
        .children()
        .find(|n| n.is_element() && FILLS.contains(&n.local()))
}

/// The paint a fill element describes: `Some(None)` for no fill.
pub fn paint(
    fill: Node,
    theme: &Theme,
    part: &Part,
    placeholder: Option<Color>,
) -> Option<Option<Paint>> {
    match fill.local() {
        "noFill" => Some(None),
        "solidFill" => Some(theme.color(fill, placeholder).map(Paint::Solid)),
        "gradFill" => {
            let mut stops: Vec<(f32, Color)> = fill
                .child("gsLst")
                .map(|list| {
                    list.kids("gs")
                        .filter_map(|gs| {
                            let pos = gs.attribute("pos")?.parse::<f32>().ok()? / 100_000.0;
                            Some((pos, theme.color(gs, placeholder)?))
                        })
                        .collect()
                })
                .unwrap_or_default();
            stops.sort_by(|a, b| a.0.total_cmp(&b.0));
            if stops.is_empty() {
                return Some(None);
            }
            let angle = fill
                .child("lin")
                .and_then(|l| l.attribute("ang"))
                .and_then(|a| a.parse::<f32>().ok())
                .map_or(90.0, |a| a / 60_000.0);
            let radial = fill
                .child("path")
                .is_some_and(|p| matches!(p.attribute("path"), Some("circle" | "shape")));
            Some(Some(Paint::Gradient {
                stops,
                angle,
                radial,
            }))
        }
        "blipFill" => Some(
            blip(fill)
                .and_then(|id| part.picture(id))
                .map(Paint::Picture),
        ),
        "pattFill" => Some(
            fill.child("fgClr")
                .and_then(|c| theme.color(c, placeholder))
                .map(Paint::Solid),
        ),
        // A group's fill: not followed, so drawn as nothing.
        _ => Some(None),
    }
}

/// The relationship id of a blip's picture, preferring an SVG version.
pub fn blip<'a>(fill: Node<'a, '_>) -> Option<&'a str> {
    let blip = fill.child("blip")?;
    let svg = blip
        .descendants()
        .find(|n| n.has_tag_name_local("svgBlip"))
        .and_then(|n| n.attr("embed"));
    svg.or_else(|| blip.attr("embed"))
}

/// The `fill` and `fill-opacity` attributes for a paint, adding any
/// gradient it needs to the canvas.
pub fn fill_attrs(canvas: &mut Canvas, paint: Option<&Paint>) -> String {
    match paint {
        None => r#" fill="none""#.into(),
        Some(Paint::Solid(c)) => color_attrs("fill", *c),
        Some(Paint::Gradient {
            stops,
            angle,
            radial,
        }) => {
            let id = canvas.id("g");
            let mut stop_svg = String::new();
            for (pos, c) in stops {
                let _ = write!(
                    stop_svg,
                    r#"<stop offset="{pos:.3}" stop-color="{}" stop-opacity="{:.3}"/>"#,
                    c.css(),
                    c.alpha
                );
            }
            if *radial {
                canvas.def(&format!(
                    r#"<radialGradient id="{id}">{stop_svg}</radialGradient>"#
                ));
            } else {
                canvas.def(&format!(
                    r#"<linearGradient id="{id}" gradientTransform="rotate({angle:.2} 0.5 0.5)">{stop_svg}</linearGradient>"#
                ));
            }
            format!(r#" fill="url(#{id})""#)
        }
        Some(Paint::Picture(_)) => r#" fill="none""#.into(),
    }
}

fn color_attrs(attr: &str, c: Color) -> String {
    if c.alpha < 1.0 {
        format!(r#" {attr}="{}" {attr}-opacity="{:.3}""#, c.css(), c.alpha)
    } else {
        format!(r#" {attr}="{}""#, c.css())
    }
}

/// An outline.
#[derive(Debug, Clone)]
pub struct Stroke {
    pub color: Color,
    pub width: f32,
    pub dash: Option<&'static str>,
    pub head: bool,
    pub tail: bool,
}

/// The outline a line element (`a:ln`) describes; `None` for no line.
pub fn stroke(ln: Node, theme: &Theme, placeholder: Option<Color>) -> Option<Stroke> {
    let fill = fill_node(ln);
    let color = match fill {
        Some(f) if f.local() == "noFill" => return None,
        Some(f) if f.local() == "solidFill" => theme.color(f, placeholder),
        Some(f) if f.local() == "gradFill" => f
            .path(&["gsLst", "gs"])
            .and_then(|gs| theme.color(gs, placeholder)),
        _ => placeholder,
    }?;
    let width = ln.attribute("w").and_then(emu).unwrap_or(0.75).max(0.25);
    let dash = ln
        .child("prstDash")
        .and_then(|d| d.attribute("val"))
        .and_then(|v| match v {
            "dash" | "lgDash" | "sysDash" => Some("4 3"),
            "dot" | "sysDot" => Some("1 2"),
            "dashDot" | "lgDashDot" | "sysDashDot" => Some("4 2 1 2"),
            _ => None,
        });
    let arrow = |name: &str| {
        ln.child(name)
            .and_then(|e| e.attribute("type"))
            .is_some_and(|t| t != "none")
    };
    Some(Stroke {
        color,
        width,
        dash,
        head: arrow("headEnd"),
        tail: arrow("tailEnd"),
    })
}

pub fn stroke_attrs(canvas: &mut Canvas, stroke: Option<&Stroke>) -> String {
    let Some(s) = stroke else {
        return r#" stroke="none""#.into();
    };
    let mut out = color_attrs("stroke", s.color);
    let _ = write!(out, r#" stroke-width="{:.2}""#, s.width);
    if let Some(dash) = s.dash {
        let scaled: Vec<String> = dash
            .split(' ')
            .filter_map(|d| d.parse::<f32>().ok())
            .map(|d| format!("{:.2}", d * s.width))
            .collect();
        let _ = write!(out, r#" stroke-dasharray="{}""#, scaled.join(" "));
    }
    for (end, wanted) in [("marker-start", s.head), ("marker-end", s.tail)] {
        if wanted {
            let id = canvas.id("m");
            canvas.def(&format!(
                r#"<marker id="{id}" viewBox="0 0 10 10" refX="5" refY="5" markerWidth="4" markerHeight="4" orient="auto-start-reverse"><path d="M0 0 L10 5 L0 10 z"{}/></marker>"#,
                color_attrs("fill", s.color)
            ));
            let _ = write!(out, r#" {end}="url(#{id})""#);
        }
    }
    out
}

/// An adjust value from a preset shape's `avLst`, as a share of 100,000.
fn adjust(geom: Node, name: &str, default: f32) -> f32 {
    geom.child("avLst")
        .and_then(|l| {
            l.kids("gd")
                .find(|g| g.attribute("name") == Some(name))
                .and_then(|g| g.attribute("fmla"))
                .and_then(|f| f.strip_prefix("val "))
                .and_then(|v| v.trim().parse::<f32>().ok())
        })
        .unwrap_or(default)
        / 100_000.0
}

/// The outline of a shape, as SVG path data in its own box from (0, 0).
/// Unknown preset shapes are drawn as rectangles.
pub fn geometry(props: Option<Node>, w: f32, h: f32) -> String {
    let rect = format!("M0 0 H{w:.2} V{h:.2} H0 Z");
    let Some(props) = props else { return rect };
    if let Some(custom) = props.child("custGeom") {
        return custom_path(custom, w, h).unwrap_or(rect);
    }
    let Some(geom) = props.child("prstGeom") else {
        return rect;
    };
    let ss = w.min(h);
    let poly = |points: &[(f32, f32)]| {
        let mut d = String::new();
        for (i, (x, y)) in points.iter().enumerate() {
            let _ = write!(d, "{}{x:.2} {y:.2} ", if i == 0 { "M" } else { "L" });
        }
        d.push('Z');
        d
    };
    match geom.attribute("prst").unwrap_or("rect") {
        "roundRect" => {
            let r = (ss * adjust(geom, "adj", 16_667.0)).min(ss / 2.0);
            format!(
                "M{r:.2} 0 H{:.2} A{r:.2} {r:.2} 0 0 1 {w:.2} {r:.2} V{:.2} A{r:.2} {r:.2} 0 0 1 {:.2} {h:.2} H{r:.2} A{r:.2} {r:.2} 0 0 1 0 {:.2} V{r:.2} A{r:.2} {r:.2} 0 0 1 {r:.2} 0 Z",
                w - r,
                h - r,
                w - r,
                h - r
            )
        }
        "ellipse" | "flowChartConnector" | "donut" => {
            let (rx, ry) = (w / 2.0, h / 2.0);
            format!(
                "M0 {ry:.2} A{rx:.2} {ry:.2} 0 1 0 {w:.2} {ry:.2} A{rx:.2} {ry:.2} 0 1 0 0 {ry:.2} Z"
            )
        }
        "line" | "straightConnector1" | "bentConnector2" | "bentConnector3"
        | "curvedConnector3" => format!("M0 0 L{w:.2} {h:.2}"),
        "triangle" | "flowChartExtract" => {
            poly(&[(w * adjust(geom, "adj", 50_000.0), 0.0), (w, h), (0.0, h)])
        }
        "rtTriangle" => poly(&[(0.0, 0.0), (w, h), (0.0, h)]),
        "diamond" | "flowChartDecision" => {
            poly(&[(w / 2.0, 0.0), (w, h / 2.0), (w / 2.0, h), (0.0, h / 2.0)])
        }
        "parallelogram" | "flowChartInputOutput" => {
            let a = ss * adjust(geom, "adj", 25_000.0);
            poly(&[(a, 0.0), (w, 0.0), (w - a, h), (0.0, h)])
        }
        "trapezoid" => {
            let a = ss * adjust(geom, "adj", 25_000.0);
            poly(&[(a, 0.0), (w - a, 0.0), (w, h), (0.0, h)])
        }
        "hexagon" => {
            let a = ss * adjust(geom, "adj", 25_000.0);
            poly(&[
                (a, 0.0),
                (w - a, 0.0),
                (w, h / 2.0),
                (w - a, h),
                (a, h),
                (0.0, h / 2.0),
            ])
        }
        "octagon" => {
            let a = ss * adjust(geom, "adj", 29_289.0);
            poly(&[
                (a, 0.0),
                (w - a, 0.0),
                (w, a),
                (w, h - a),
                (w - a, h),
                (a, h),
                (0.0, h - a),
                (0.0, a),
            ])
        }
        "homePlate" => {
            let a = ss * adjust(geom, "adj", 50_000.0);
            poly(&[(0.0, 0.0), (w - a, 0.0), (w, h / 2.0), (w - a, h), (0.0, h)])
        }
        "chevron" => {
            let a = ss * adjust(geom, "adj", 50_000.0);
            poly(&[
                (0.0, 0.0),
                (w - a, 0.0),
                (w, h / 2.0),
                (w - a, h),
                (0.0, h),
                (a, h / 2.0),
            ])
        }
        "rightArrow" => {
            let shaft = h * adjust(geom, "adj1", 50_000.0);
            let head = (ss * adjust(geom, "adj2", 50_000.0)).min(w);
            let (t, b) = ((h - shaft) / 2.0, (h + shaft) / 2.0);
            poly(&[
                (0.0, t),
                (w - head, t),
                (w - head, 0.0),
                (w, h / 2.0),
                (w - head, h),
                (w - head, b),
                (0.0, b),
            ])
        }
        "leftArrow" => {
            let shaft = h * adjust(geom, "adj1", 50_000.0);
            let head = (ss * adjust(geom, "adj2", 50_000.0)).min(w);
            let (t, b) = ((h - shaft) / 2.0, (h + shaft) / 2.0);
            poly(&[
                (w, t),
                (head, t),
                (head, 0.0),
                (0.0, h / 2.0),
                (head, h),
                (head, b),
                (w, b),
            ])
        }
        "upArrow" | "downArrow" => {
            let shaft = w * adjust(geom, "adj1", 50_000.0);
            let head = (ss * adjust(geom, "adj2", 50_000.0)).min(h);
            let (l, r) = ((w - shaft) / 2.0, (w + shaft) / 2.0);
            let up = [
                (l, h),
                (l, head),
                (0.0, head),
                (w / 2.0, 0.0),
                (w, head),
                (r, head),
                (r, h),
            ];
            if geom.attribute("prst") == Some("upArrow") {
                poly(&up)
            } else {
                poly(&up.map(|(x, y)| (x, h - y)))
            }
        }
        "plus" | "mathPlus" => {
            let a = ss * adjust(geom, "adj", 25_000.0);
            poly(&[
                (a, 0.0),
                (w - a, 0.0),
                (w - a, a),
                (w, a),
                (w, h - a),
                (w - a, h - a),
                (w - a, h),
                (a, h),
                (a, h - a),
                (0.0, h - a),
                (0.0, a),
                (a, a),
            ])
        }
        "star5" => {
            let mut points = Vec::new();
            for i in 0..10 {
                let r = if i % 2 == 0 { 0.5 } else { 0.19 };
                let a = -PI / 2.0 + i as f32 * PI / 5.0;
                points.push((w / 2.0 + r * w * a.cos(), h / 2.0 + r * h * a.sin()));
            }
            poly(&points)
        }
        "snip1Rect" | "snipRoundRect" => {
            let a = ss * adjust(geom, "adj", 16_667.0);
            poly(&[(0.0, 0.0), (w - a, 0.0), (w, a), (w, h), (0.0, h)])
        }
        "round2SameRect" => {
            let r = (ss * adjust(geom, "adj1", 16_667.0)).min(ss / 2.0);
            format!(
                "M{r:.2} 0 H{:.2} A{r:.2} {r:.2} 0 0 1 {w:.2} {r:.2} V{h:.2} H0 V{r:.2} A{r:.2} {r:.2} 0 0 1 {r:.2} 0 Z",
                w - r
            )
        }
        "wedgeRectCallout" | "wedgeRoundRectCallout" | "cloudCallout" => rect,
        _ => rect,
    }
}

/// A custom shape's paths, scaled from their own coordinates to the box.
fn custom_path(custom: Node, w: f32, h: f32) -> Option<String> {
    let mut d = String::new();
    for path in custom.child("pathLst")?.kids("path") {
        let pw = path
            .attribute("w")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(w * 12_700.0);
        let ph = path
            .attribute("h")
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(h * 12_700.0);
        let (sx, sy) = (w / pw, h / ph);
        let point = |pt: Node| -> Option<(f32, f32)> {
            Some((
                pt.attribute("x")?.parse::<f32>().ok()? * sx,
                pt.attribute("y")?.parse::<f32>().ok()? * sy,
            ))
        };
        let mut at = (0.0, 0.0);
        for step in path.children().filter(|n| n.is_element()) {
            let pts: Vec<(f32, f32)> = step.kids("pt").filter_map(point).collect();
            match step.local() {
                "moveTo" => {
                    let (x, y) = *pts.first()?;
                    let _ = write!(d, "M{x:.2} {y:.2} ");
                    at = (x, y);
                }
                "lnTo" => {
                    let (x, y) = *pts.first()?;
                    let _ = write!(d, "L{x:.2} {y:.2} ");
                    at = (x, y);
                }
                "cubicBezTo" if pts.len() == 3 => {
                    let _ = write!(
                        d,
                        "C{:.2} {:.2} {:.2} {:.2} {:.2} {:.2} ",
                        pts[0].0, pts[0].1, pts[1].0, pts[1].1, pts[2].0, pts[2].1
                    );
                    at = pts[2];
                }
                "quadBezTo" if pts.len() == 2 => {
                    let _ = write!(
                        d,
                        "Q{:.2} {:.2} {:.2} {:.2} ",
                        pts[0].0, pts[0].1, pts[1].0, pts[1].1
                    );
                    at = pts[1];
                }
                "arcTo" => {
                    let num = |n: &str| step.attribute(n).and_then(|v| v.parse::<f32>().ok());
                    let (wr, hr) = (num("wR")? * sx, num("hR")? * sy);
                    let start = num("stAng")? / 60_000.0 * PI / 180.0;
                    let sweep = num("swAng")? / 60_000.0 * PI / 180.0;
                    let center = (at.0 - wr * start.cos(), at.1 - hr * start.sin());
                    let end = (
                        center.0 + wr * (start + sweep).cos(),
                        center.1 + hr * (start + sweep).sin(),
                    );
                    let large = u8::from(sweep.abs() > PI);
                    let positive = u8::from(sweep > 0.0);
                    let _ = write!(
                        d,
                        "A{wr:.2} {hr:.2} 0 {large} {positive} {:.2} {:.2} ",
                        end.0, end.1
                    );
                    at = end;
                }
                "close" => d.push_str("Z "),
                _ => {}
            }
        }
    }
    (!d.is_empty()).then_some(d)
}

/// A shape's position and size from an `xfrm` element.
pub fn xfrm(node: Node) -> Option<(Rect, f32, bool, bool)> {
    let off = node.child("off")?;
    let ext = node.child("ext")?;
    let rect = Rect {
        x: off.attribute("x").and_then(emu)?,
        y: off.attribute("y").and_then(emu)?,
        w: ext.attribute("cx").and_then(emu)?,
        h: ext.attribute("cy").and_then(emu)?,
    };
    let rot = node
        .attribute("rot")
        .and_then(|r| r.parse::<f32>().ok())
        .map_or(0.0, |r| r / 60_000.0);
    let flip = |n: &str| matches!(node.attribute(n), Some("1" | "true"));
    Some((rect, rot, flip("flipH"), flip("flipV")))
}

/// Draws a picture over a box, cropped by `srcRect`, clipped to `clip`.
pub fn picture(canvas: &mut Canvas, uri: &str, rect: Rect, crop: Option<Node>, clip: &str) {
    let share = |n: &str| {
        crop.and_then(|c| c.attribute(n))
            .and_then(|v| v.parse::<f32>().ok())
            .map_or(0.0, |v| v / 100_000.0)
    };
    let (l, t, r, b) = (share("l"), share("t"), share("r"), share("b"));
    let full_w = rect.w / (1.0 - l - r).max(0.01);
    let full_h = rect.h / (1.0 - t - b).max(0.01);
    let id = canvas.id("c");
    canvas.def(&format!(
        r#"<clipPath id="{id}"><path transform="translate({:.2} {:.2})" d="{clip}"/></clipPath>"#,
        rect.x, rect.y
    ));
    let _ = write!(
        canvas.body,
        r#"<image clip-path="url(#{id})" x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" preserveAspectRatio="none" xlink:href="{uri}"/>"#,
        rect.x - l * full_w,
        rect.y - t * full_h,
        full_w,
        full_h
    );
}

// ---------------------------------------------------------------------------
// Text.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Bullet {
    None,
    Char(String),
    /// Automatic numbering, by its scheme name, starting at a number.
    Number(String, u32),
}

/// Spacing given as points or as a share of a line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Spacing {
    Points(f32),
    Lines(f32),
}

/// What a run of text inherits; unset fields fall through to the next level.
#[derive(Debug, Clone, Default)]
pub struct RunProps {
    pub size: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    pub color: Option<Color>,
    pub font: Option<String>,
    pub baseline: Option<f32>,
    pub caps: Option<bool>,
}

impl RunProps {
    pub fn apply(&mut self, rpr: Node, theme: &Theme, placeholder: Option<Color>) {
        if let Some(sz) = rpr.attribute("sz").and_then(|v| v.parse::<f32>().ok()) {
            self.size = Some(sz / 100.0);
        }
        let flag = |n: &str| rpr.attribute(n).map(|v| v == "1" || v == "true");
        if let Some(b) = flag("b") {
            self.bold = Some(b);
        }
        if let Some(i) = flag("i") {
            self.italic = Some(i);
        }
        if let Some(u) = rpr.attribute("u") {
            self.underline = Some(u != "none");
        }
        if let Some(s) = rpr.attribute("strike") {
            self.strike = Some(s != "noStrike");
        }
        if let Some(cap) = rpr.attribute("cap") {
            self.caps = Some(cap == "all");
        }
        if let Some(b) = rpr
            .attribute("baseline")
            .and_then(|v| v.parse::<f32>().ok())
        {
            self.baseline = Some(b / 100_000.0);
        }
        if let Some(fill) = rpr
            .child("solidFill")
            .and_then(|f| theme.color(f, placeholder))
        {
            self.color = Some(fill);
        }
        if let Some(face) = rpr
            .child("latin")
            .and_then(|l| l.attribute("typeface"))
            .filter(|t| !t.is_empty())
        {
            self.font = Some(theme.typeface(face).to_owned());
        }
    }

    pub fn merged(&self, over: &RunProps) -> RunProps {
        RunProps {
            size: over.size.or(self.size),
            bold: over.bold.or(self.bold),
            italic: over.italic.or(self.italic),
            underline: over.underline.or(self.underline),
            strike: over.strike.or(self.strike),
            color: over.color.or(self.color),
            font: over.font.clone().or_else(|| self.font.clone()),
            baseline: over.baseline.or(self.baseline),
            caps: over.caps.or(self.caps),
        }
    }

    pub fn style(&self, defaults: &TextDefaults, scale: f32) -> Style {
        Style {
            family: self.font.clone().unwrap_or_else(|| defaults.font.clone()),
            size: self.size.unwrap_or(defaults.size) * scale,
            bold: self.bold.unwrap_or(defaults.bold),
            italic: self.italic.unwrap_or(false),
            underline: self.underline.unwrap_or(false),
            strike: self.strike.unwrap_or(false),
            color: self.color.unwrap_or(defaults.color),
            baseline: self.baseline.unwrap_or(0.0),
            caps: self.caps.unwrap_or(false),
        }
    }
}

/// What a paragraph inherits.
#[derive(Debug, Clone, Default)]
pub struct ParaProps {
    pub align: Option<Align>,
    pub margin: Option<f32>,
    pub indent: Option<f32>,
    pub bullet: Option<Bullet>,
    pub bullet_font: Option<String>,
    pub bullet_color: Option<Color>,
    pub bullet_size: Option<f32>,
    pub before: Option<Spacing>,
    pub after: Option<Spacing>,
    pub line: Option<Spacing>,
    pub run: RunProps,
}

fn spacing(node: Option<Node>) -> Option<Spacing> {
    let node = node?;
    if let Some(p) = node.child("spcPts").and_then(|p| p.attribute("val")) {
        return p.parse::<f32>().ok().map(|v| Spacing::Points(v / 100.0));
    }
    node.child("spcPct")
        .and_then(|p| p.attribute("val"))
        .and_then(|v| v.parse::<f32>().ok())
        .map(|v| Spacing::Lines(v / 100_000.0))
}

impl ParaProps {
    /// Applies a paragraph properties element (`pPr` or `lvlNpPr`).
    pub fn apply(&mut self, ppr: Node, theme: &Theme, placeholder: Option<Color>) {
        if let Some(a) = ppr.attribute("algn") {
            self.align = Some(match a {
                "ctr" => Align::Center,
                "r" => Align::Right,
                _ => Align::Left,
            });
        }
        if let Some(m) = ppr.attribute("marL").and_then(emu) {
            self.margin = Some(m);
        }
        if let Some(i) = ppr.attribute("indent").and_then(emu) {
            self.indent = Some(i);
        }
        if let Some(s) = spacing(ppr.child("spcBef")) {
            self.before = Some(s);
        }
        if let Some(s) = spacing(ppr.child("spcAft")) {
            self.after = Some(s);
        }
        if let Some(s) = spacing(ppr.child("lnSpc")) {
            self.line = Some(s);
        }
        for child in ppr.children().filter(|n| n.is_element()) {
            match child.local() {
                "buNone" => self.bullet = Some(Bullet::None),
                "buChar" => {
                    self.bullet = Some(Bullet::Char(
                        child.attribute("char").unwrap_or("•").to_owned(),
                    ))
                }
                "buAutoNum" => {
                    self.bullet = Some(Bullet::Number(
                        child.attribute("type").unwrap_or("arabicPeriod").to_owned(),
                        child
                            .attribute("startAt")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(1),
                    ))
                }
                "buFont" => self.bullet_font = child.attribute("typeface").map(str::to_owned),
                "buClr" => self.bullet_color = theme.color(child, placeholder),
                "buSzPct" => {
                    self.bullet_size = child
                        .attribute("val")
                        .and_then(|v| v.parse::<f32>().ok())
                        .map(|v| v / 100_000.0)
                }
                "defRPr" => self.run.apply(child, theme, placeholder),
                _ => {}
            }
        }
    }
}

/// Defaults for text that inherits nothing.
#[derive(Debug, Clone)]
pub struct TextDefaults {
    pub font: String,
    pub size: f32,
    pub color: Color,
    pub bold: bool,
    /// The slide's number, for slide-number fields.
    pub number: usize,
}

/// A text body's box properties, gathered from `bodyPr` elements, the most
/// specific last.
#[derive(Debug, Clone)]
pub struct BodyProps {
    pub insets: [f32; 4],
    pub anchor: char,
    pub wrap: bool,
    pub font_scale: f32,
    pub spacing_cut: f32,
    pub center: bool,
}

impl Default for BodyProps {
    fn default() -> Self {
        BodyProps {
            insets: [7.2, 3.6, 7.2, 3.6],
            anchor: 't',
            wrap: true,
            font_scale: 1.0,
            spacing_cut: 0.0,
            center: false,
        }
    }
}

impl BodyProps {
    pub fn apply(&mut self, body: Node) {
        for (i, name) in ["lIns", "tIns", "rIns", "bIns"].iter().enumerate() {
            if let Some(v) = body.attribute(*name).and_then(emu) {
                self.insets[i] = v;
            }
        }
        if let Some(a) = body.attribute("anchor") {
            self.anchor = match a {
                "ctr" => 'c',
                "b" => 'b',
                _ => 't',
            };
        }
        if let Some(c) = body.attribute("anchorCtr") {
            self.center = c == "1";
        }
        if let Some(w) = body.attribute("wrap") {
            self.wrap = w != "none";
        }
        if let Some(fit) = body.child("normAutofit") {
            self.font_scale = fit
                .attribute("fontScale")
                .and_then(|v| v.parse::<f32>().ok())
                .map_or(1.0, |v| v / 100_000.0);
            self.spacing_cut = fit
                .attribute("lnSpcReduction")
                .and_then(|v| v.parse::<f32>().ok())
                .map_or(0.0, |v| v / 100_000.0);
        }
    }
}

/// The inheritance chain for a text body's paragraphs: list styles (each
/// with `lvl1pPr`…`lvl9pPr`), least specific first.
pub struct Cascade<'a, 'i> {
    pub lists: Vec<Node<'a, 'i>>,
    /// A shape's own text color (from its style), and how many of `lists`
    /// it overrides: the inherited ones, not the shape's own.
    pub color: Option<(usize, Color)>,
}

impl Cascade<'_, '_> {
    pub fn level(&self, level: usize, theme: &Theme, placeholder: Option<Color>) -> ParaProps {
        let mut props = ParaProps::default();
        let name = format!("lvl{}pPr", level + 1);
        for (i, list) in self.lists.iter().enumerate() {
            if let Some((after, color)) = self.color
                && after == i
            {
                props.run.color = Some(color);
            }
            if let Some(ppr) = list.children().find(|n| n.has_tag_name_local(&name)) {
                props.apply(ppr, theme, placeholder);
            }
        }
        if let Some((after, color)) = self.color
            && after >= self.lists.len()
        {
            props.run.color = Some(color);
        }
        props
    }
}

/// The number shown for item `n` (counting from 1) of a numbering scheme.
pub fn number_label(scheme: &str, n: u32) -> String {
    let core = if scheme.starts_with("alphaLc") {
        letters(n).to_lowercase()
    } else if scheme.starts_with("alphaUc") {
        letters(n)
    } else if scheme.starts_with("romanLc") {
        roman(n).to_lowercase()
    } else if scheme.starts_with("romanUc") {
        roman(n)
    } else {
        n.to_string()
    };
    if scheme.ends_with("ParenBoth") {
        format!("({core})")
    } else if scheme.ends_with("ParenR") {
        format!("{core})")
    } else if scheme.ends_with("Plain") {
        core
    } else {
        format!("{core}.")
    }
}

fn letters(mut n: u32) -> String {
    let mut out = Vec::new();
    while n > 0 {
        n -= 1;
        out.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn roman(mut n: u32) -> String {
    let table = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for (value, numeral) in table {
        while n >= value {
            out.push_str(numeral);
            n -= value;
        }
    }
    out
}

/// A paragraph laid out for drawing.
struct LaidPara {
    lines: Vec<typeset::Line>,
    /// Where the first line and later lines start, from the text box's left.
    first_x: f32,
    rest_x: f32,
    width: f32,
    align: Align,
    before: f32,
    after: f32,
    line_factor: Option<f32>,
    line_exact: Option<f32>,
    bullet: Option<(typeset::Line, f32)>,
}

/// The words of a paragraph, with fields filled in.
pub fn paragraph_runs(
    p: Node,
    para: &ParaProps,
    theme: &Theme,
    defaults: &TextDefaults,
    scale: f32,
    placeholder: Option<Color>,
) -> Vec<Run> {
    let mut runs = Vec::new();
    for child in p.children().filter(|n| n.is_element()) {
        let (text, rpr) = match child.local() {
            "r" => (
                child
                    .child("t")
                    .and_then(|t| t.text())
                    .unwrap_or("")
                    .to_owned(),
                child.child("rPr"),
            ),
            "br" => ("\n".to_owned(), child.child("rPr")),
            "fld" => {
                let cached = child
                    .child("t")
                    .and_then(|t| t.text())
                    .unwrap_or("")
                    .to_owned();
                let text = if child.attribute("type") == Some("slidenum") {
                    defaults.number.to_string()
                } else {
                    cached
                };
                (text, child.child("rPr"))
            }
            _ => continue,
        };
        let mut props = RunProps::default();
        if let Some(rpr) = rpr {
            props.apply(rpr, theme, placeholder);
        }
        let style = para.run.merged(&props).style(defaults, scale);
        runs.push(Run {
            text,
            style: Rc::new(style),
        });
    }
    runs
}

/// A text body laid out in its box, ready to draw.
pub struct LaidBody {
    paras: Vec<LaidPara>,
    inner: Rect,
    anchor: char,
    /// Height of the text, without the box's insets.
    pub total: f32,
    /// Top and bottom insets.
    pub insets: f32,
}

impl LaidPara {
    fn pitch(&self, line: &typeset::Line) -> f32 {
        match (self.line_exact, self.line_factor) {
            (Some(exact), _) => exact,
            (_, Some(f)) => line.height() * f,
            _ => line.height(),
        }
    }
}

/// Lays out a text body inside `rect`.
#[allow(clippy::too_many_arguments)]
pub fn lay_body(
    body: Node,
    rect: Rect,
    cascade: &Cascade,
    props: &BodyProps,
    theme: &Theme,
    defaults: &TextDefaults,
    placeholder: Option<Color>,
) -> LaidBody {
    let [l, t, r, b] = props.insets;
    let inner = Rect {
        x: rect.x + l,
        y: rect.y + t,
        w: (rect.w - l - r).max(1.0),
        h: (rect.h - t - b).max(0.0),
    };
    let scale = props.font_scale;
    let mut laid = Vec::new();
    let mut counters: Vec<u32> = vec![0; 9];
    for p in body.kids("p") {
        let ppr = p.child("pPr");
        let level = ppr
            .and_then(|n| n.attribute("lvl"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
            .min(8);
        let mut para = cascade.level(level, theme, placeholder);
        if let Some(ppr) = ppr {
            para.apply(ppr, theme, placeholder);
        }
        // An end-of-paragraph run's size sets an empty paragraph's height.
        let mut end = para.run.clone();
        if let Some(end_rpr) = p.child("endParaRPr") {
            end.apply(end_rpr, theme, placeholder);
        }
        let runs = paragraph_runs(p, &para, theme, defaults, scale, placeholder);
        let has_text = runs.iter().any(|r| !r.text.trim().is_empty());
        let first_style = runs
            .first()
            .map(|r| (*r.style).clone())
            .unwrap_or_else(|| end.style(defaults, scale));
        let margin = para.margin.unwrap_or(0.0);
        let indent = para.indent.unwrap_or(0.0);
        let bullet_text = match (&para.bullet, has_text) {
            (Some(Bullet::Char(c)), true) => {
                counters[level] = 0;
                Some(c.clone())
            }
            (Some(Bullet::Number(scheme, start)), true) => {
                let n = start + counters[level];
                counters[level] += 1;
                Some(number_label(scheme, n))
            }
            _ => {
                if has_text {
                    counters[level] = 0;
                }
                None
            }
        };
        let bullet = bullet_text.map(|text| {
            let style = Style {
                family: para
                    .bullet_font
                    .clone()
                    .filter(|f| !f.starts_with('+'))
                    .unwrap_or_else(|| first_style.family.clone()),
                size: first_style.size * para.bullet_size.unwrap_or(1.0),
                color: para.bullet_color.unwrap_or(first_style.color),
                underline: false,
                strike: false,
                ..first_style.clone()
            };
            let line = typeset::wrap(
                &[Run {
                    text,
                    style: Rc::new(style.clone()),
                }],
                f32::MAX,
                f32::MAX,
                &style,
            )
            .remove(0);
            (line, margin + indent)
        });
        let first_x = match &bullet {
            Some((line, at)) => (margin).max(at + line.width + 2.0),
            None => margin + indent,
        };
        let width = if props.wrap { inner.w } else { f32::MAX / 4.0 };
        let lines = typeset::wrap(
            &runs,
            (width - first_x).max(1.0),
            (width - margin).max(1.0),
            &end.style(defaults, scale),
        );
        let (line_factor, line_exact) = match para.line {
            Some(Spacing::Lines(f)) => (Some((f - props.spacing_cut).max(0.5)), None),
            Some(Spacing::Points(p)) => (None, Some(p * scale)),
            None => (Some((1.0 - props.spacing_cut).max(0.5)), None),
        };
        let size = first_style.size;
        let resolve = |s: Option<Spacing>| match s {
            Some(Spacing::Points(p)) => p,
            Some(Spacing::Lines(f)) => f * size * 1.2,
            None => 0.0,
        };
        laid.push(LaidPara {
            lines,
            first_x,
            rest_x: margin,
            width: inner.w,
            align: para.align.unwrap_or_default(),
            before: resolve(para.before),
            after: resolve(para.after),
            line_factor,
            line_exact,
            bullet,
        });
    }
    let total: f32 = laid
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let before = if i == 0 { 0.0 } else { p.before };
            before + p.after + p.lines.iter().map(|l| p.pitch(l)).sum::<f32>()
        })
        .sum();
    LaidBody {
        paras: laid,
        inner,
        anchor: props.anchor,
        total,
        insets: t + b,
    }
}

/// Draws a laid-out text body.
pub fn draw_body(canvas: &mut Canvas, body: &LaidBody) {
    let (inner, total) = (body.inner, body.total);
    let props_anchor = body.anchor;
    let mut y = match props_anchor {
        'c' => inner.y + (inner.h - total) / 2.0,
        'b' => inner.y + inner.h - total,
        _ => inner.y,
    };
    for (i, p) in body.paras.iter().enumerate() {
        if i > 0 {
            y += p.before;
        }
        for (n, line) in p.lines.iter().enumerate() {
            let step = p.pitch(line);
            let baseline = y + step - line.descent - (step - line.height()).min(0.0) * 0.0;
            let start = if n == 0 { p.first_x } else { p.rest_x };
            let room = p.width - start;
            let x = inner.x
                + start
                + match p.align {
                    Align::Left => 0.0,
                    Align::Center => (room - line.width) / 2.0,
                    Align::Right => room - line.width,
                };
            if n == 0
                && let Some((bullet, at)) = &p.bullet
            {
                let bx = match p.align {
                    Align::Left => inner.x + at,
                    _ => x - bullet.width - 4.0,
                };
                typeset::svg_line(&mut canvas.body, bullet, bx, baseline);
            }
            typeset::svg_line(&mut canvas.body, line, x, baseline);
            y += step;
        }
        y += p.after;
    }
}

/// Draws a text body inside `rect`.
#[allow(clippy::too_many_arguments)]
pub fn text_body(
    canvas: &mut Canvas,
    body: Node,
    rect: Rect,
    cascade: &Cascade,
    props: &BodyProps,
    theme: &Theme,
    defaults: &TextDefaults,
    placeholder: Option<Color>,
) {
    let laid = lay_body(body, rect, cascade, props, theme, defaults, placeholder);
    draw_body(canvas, &laid);
}

/// The text of a text body as markdown: titles as headings, bulleted
/// paragraphs as list items.
pub fn body_markdown(
    body: Node,
    heading: Option<usize>,
    cascade: &Cascade,
    theme: &Theme,
) -> String {
    let mut out = Vec::new();
    for p in body.kids("p") {
        let ppr = p.child("pPr");
        let level = ppr
            .and_then(|n| n.attribute("lvl"))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
            .min(8);
        let mut para = cascade.level(level, theme, None);
        if let Some(ppr) = ppr {
            para.apply(ppr, theme, None);
        }
        let mut text = String::new();
        for child in p.children().filter(|n| n.is_element()) {
            match child.local() {
                "r" | "fld" => {
                    let t = child.child("t").and_then(|t| t.text()).unwrap_or("");
                    let mut props = para.run.clone();
                    if let Some(rpr) = child.child("rPr") {
                        props.apply(rpr, theme, None);
                    }
                    let escaped = crate::ooxml::md_escape(t);
                    let trimmed = escaped.trim();
                    if trimmed.is_empty() || heading.is_some() {
                        text.push_str(&escaped);
                    } else {
                        let lead = &escaped[..escaped.len() - escaped.trim_start().len()];
                        let tail = &escaped[escaped.trim_end().len()..];
                        let mark = match (props.bold == Some(true), props.italic == Some(true)) {
                            (true, true) => "***",
                            (true, false) => "**",
                            (false, true) => "*",
                            (false, false) => "",
                        };
                        text.push_str(&format!("{lead}{mark}{trimmed}{mark}{tail}"));
                    }
                }
                "br" => text.push_str("  \n"),
                _ => {}
            }
        }
        let text = text.trim().to_owned();
        if text.is_empty() {
            continue;
        }
        match heading {
            Some(depth) => out.push(format!("{} {}", "#".repeat(depth), text)),
            None => match &para.bullet {
                Some(Bullet::Char(_)) => out.push(format!("{}- {text}", "  ".repeat(level))),
                Some(Bullet::Number(..)) => out.push(format!("{}1. {text}", "   ".repeat(level))),
                _ => out.push(text),
            },
        }
    }
    let mut md = String::new();
    for (i, line) in out.iter().enumerate() {
        if i > 0 {
            let list = |l: &str| {
                let t = l.trim_start();
                t.starts_with("- ") || t.starts_with("1. ")
            };
            md.push_str(if list(line) && list(&out[i - 1]) {
                "\n"
            } else {
                "\n\n"
            });
        }
        md.push_str(line);
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbering_schemes_read_as_office_shows_them() {
        assert_eq!(number_label("arabicPeriod", 3), "3.");
        assert_eq!(number_label("alphaLcParenR", 2), "b)");
        assert_eq!(number_label("romanUcPeriod", 4), "IV.");
        assert_eq!(number_label("alphaUcPeriod", 27), "AA.");
        assert_eq!(number_label("arabicParenBoth", 1), "(1)");
    }

    #[test]
    fn preset_shapes_have_outlines() {
        let doc = roxmltree::Document::parse(
            r#"<p:spPr xmlns:p="p" xmlns:a="a"><a:prstGeom prst="ellipse"/></p:spPr>"#,
        )
        .unwrap();
        let d = geometry(Some(doc.root_element()), 100.0, 50.0);
        assert!(d.starts_with("M0 25.00 A50.00 25.00"));
        assert_eq!(geometry(None, 10.0, 5.0), "M0 0 H10.00 V5.00 H0 Z");
    }

    #[test]
    fn custom_paths_scale_to_the_box() {
        let doc = roxmltree::Document::parse(
            r#"<a:custGeom xmlns:a="a"><a:pathLst><a:path w="100" h="100"><a:moveTo><a:pt x="0" y="0"/></a:moveTo><a:lnTo><a:pt x="100" y="100"/></a:lnTo><a:close/></a:path></a:pathLst></a:custGeom>"#,
        )
        .unwrap();
        assert_eq!(
            custom_path(doc.root_element(), 20.0, 10.0).unwrap(),
            "M0.00 0.00 L20.00 10.00 Z "
        );
    }

    #[test]
    fn markdown_keeps_lists_and_emphasis() {
        let doc = roxmltree::Document::parse(
            r#"<p:txBody xmlns:p="p" xmlns:a="a"><a:p><a:pPr><a:buChar char="•"/></a:pPr><a:r><a:rPr b="1"/><a:t>Bold</a:t></a:r><a:r><a:t> point</a:t></a:r></a:p><a:p><a:pPr lvl="1"><a:buChar char="–"/></a:pPr><a:r><a:t>Sub</a:t></a:r></a:p></p:txBody>"#,
        )
        .unwrap();
        let theme = Theme::default();
        let md = body_markdown(
            doc.root_element(),
            None,
            &Cascade {
                lists: vec![],
                color: None,
            },
            &theme,
        );
        assert_eq!(md, "- **Bold** point\n  - Sub");
    }
}
