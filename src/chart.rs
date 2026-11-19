//! Office charts (`c:chartSpace`) drawn from the values cached in the file:
//! bar and column, line, area, pie and doughnut charts, with title, axis,
//! gridlines and legend. Charts of other kinds draw the first of these they
//! hold, or nothing.

use std::fmt::Write as _;
use std::rc::Rc;

use crate::drawing::{Canvas, Rect, fill_node};
use crate::ooxml::{Color, Local, Theme, md_escape, roxmltree};
use crate::typeset::{self, Run, Style};
use roxmltree::Node;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Bar { horizontal: bool, stacked: bool },
    Line,
    Area,
    Pie { hole: f32 },
}

#[derive(Debug, Clone)]
struct Series {
    name: String,
    values: Vec<Option<f64>>,
    color: Option<Color>,
    /// Pie slices' own colors, by point.
    points: Vec<(usize, Color)>,
}

#[derive(Debug, Clone)]
struct Chart {
    title: Option<String>,
    kind: Kind,
    categories: Vec<String>,
    series: Vec<Series>,
    legend: bool,
}

/// Most points a series is read with: a chart past this is past reading,
/// and a damaged file naming a huge index can't make leaf allocate for it.
const MAX_POINTS: usize = 100_000;

fn cached(node: Option<Node>) -> Vec<(usize, String)> {
    let Some(node) = node else { return Vec::new() };
    let Some(cache) = node
        .descendants()
        .find(|n| n.has_tag_name_local("strCache") || n.has_tag_name_local("numCache"))
    else {
        return Vec::new();
    };
    cache
        .kids("pt")
        .filter_map(|pt| {
            let idx = pt
                .attribute("idx")?
                .parse()
                .ok()
                .filter(|i| *i < MAX_POINTS)?;
            let v = pt.child("v")?.text().unwrap_or("").to_owned();
            Some((idx, v))
        })
        .collect()
}

fn rich_text(node: Node) -> String {
    node.descendants()
        .filter(|n| n.has_tag_name_local("t"))
        .filter_map(|n| n.text())
        .collect::<Vec<_>>()
        .join("")
}

fn parse(xml: &str, theme: &Theme) -> Option<Chart> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let chart = doc.root_element().child("chart")?;
    let plot = chart.child("plotArea")?;
    let (node, kind) = plot.children().find_map(|n| {
        let kind = match n.local() {
            "barChart" | "bar3DChart" => {
                let horizontal = n.child("barDir").and_then(|d| d.attribute("val")) == Some("bar");
                let stacked = matches!(
                    n.child("grouping").and_then(|g| g.attribute("val")),
                    Some("stacked" | "percentStacked")
                );
                Kind::Bar {
                    horizontal,
                    stacked,
                }
            }
            "lineChart" | "line3DChart" | "scatterChart" | "radarChart" => Kind::Line,
            "areaChart" | "area3DChart" => Kind::Area,
            "pieChart" | "pie3DChart" | "ofPieChart" => Kind::Pie { hole: 0.0 },
            "doughnutChart" => Kind::Pie {
                hole: n
                    .child("holeSize")
                    .and_then(|h| h.attribute("val"))
                    .and_then(|v| v.parse::<f32>().ok())
                    .map_or(0.5, |v| v / 100.0),
            },
            _ => return None,
        };
        Some((n, kind))
    })?;
    let mut categories: Vec<String> = Vec::new();
    let mut series = Vec::new();
    for ser in node.kids("ser") {
        let name = cached(ser.child("tx"))
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .or_else(|| ser.child("tx").map(rich_text))
            .unwrap_or_else(|| format!("Series {}", series.len() + 1));
        let cats = cached(ser.child("cat").or_else(|| ser.child("xVal")));
        let vals = cached(ser.child("val").or_else(|| ser.child("yVal")));
        let count = vals.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
        let mut values = vec![None; count];
        for (i, v) in vals {
            if let Some(slot) = values.get_mut(i) {
                *slot = v.trim().parse::<f64>().ok();
            }
        }
        if categories.is_empty() && !cats.is_empty() {
            let n = cats.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
            categories = vec![String::new(); n];
            for (i, v) in cats {
                categories[i] = v;
            }
        }
        let color = ser
            .child("spPr")
            .and_then(fill_node)
            .filter(|f| f.local() == "solidFill")
            .and_then(|f| theme.color(f, None));
        let points = ser
            .kids("dPt")
            .filter_map(|p| {
                let idx = p.child("idx")?.attribute("val")?.parse().ok()?;
                let fill = p.child("spPr").and_then(fill_node)?;
                Some((idx, theme.color(fill, None)?))
            })
            .collect();
        series.push(Series {
            name,
            values,
            color,
            points,
        });
    }
    if series.is_empty() {
        return None;
    }
    let longest = series.iter().map(|s| s.values.len()).max().unwrap_or(0);
    if categories.len() < longest {
        categories.extend((categories.len()..longest).map(|i| (i + 1).to_string()));
    }
    let deleted = chart
        .child("autoTitleDeleted")
        .and_then(|d| d.attribute("val"))
        == Some("1");
    // A title is written out, or taken from a cell and cached.
    let title = chart
        .child("title")
        .map(|t| {
            let rich = rich_text(t);
            if rich.trim().is_empty() {
                cached(t.child("tx"))
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                rich
            }
        })
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            (!deleted && chart.child("title").is_some() && series.len() == 1)
                .then(|| series[0].name.clone())
        });
    Some(Chart {
        title,
        kind,
        categories,
        series,
        legend: chart.child("legend").is_some(),
    })
}

fn accent(theme: &Theme, i: usize) -> Color {
    theme
        .scheme(&format!("accent{}", i % 6 + 1))
        .unwrap_or(Color::rgb(68, 114, 196))
}

/// A tidy scale for values from `low` to `high`: its ends and step.
fn nice_scale(low: f64, high: f64) -> (f64, f64, f64) {
    let (low, high) = (low.min(0.0), high.max(0.0));
    let span = (high - low).max(1e-9);
    let raw = span / 5.0;
    let magnitude = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|s| *s >= raw)
        .unwrap_or(magnitude * 10.0);
    (
        (low / step).floor() * step,
        (high / step).ceil() * step,
        step,
    )
}

fn label(value: f64) -> String {
    if value.abs() >= 1e6 {
        format!("{}M", trim(value / 1e6))
    } else if value.abs() >= 1e4 {
        format!("{}K", trim(value / 1e3))
    } else {
        trim(value)
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn text(canvas: &mut Canvas, s: &str, x: f32, y: f32, style: &Rc<Style>, anchor: &str) {
    let width = typeset::measure(s, style);
    let x = match anchor {
        "middle" => x - width / 2.0,
        "end" => x - width,
        _ => x,
    };
    let line = typeset::wrap(
        &[Run {
            text: s.to_owned(),
            style: style.clone(),
        }],
        f32::MAX,
        f32::MAX,
        style,
    );
    if let Some(line) = line.first() {
        typeset::svg_line(&mut canvas.body, line, x, y);
    }
}

/// Draws the chart in `xml` over `rect`.
pub fn draw(canvas: &mut Canvas, xml: &str, rect: Rect, theme: &Theme) {
    let Some(chart) = parse(xml, theme) else {
        return;
    };
    let ink = theme.scheme("tx1").unwrap_or(Color::BLACK);
    let muted = Color { alpha: 0.75, ..ink };
    let base = rect.h.min(rect.w).clamp(80.0, 600.0);
    let small = Rc::new(Style {
        family: theme.minor.clone(),
        size: (base / 26.0).clamp(7.0, 14.0),
        color: muted,
        ..Style::default()
    });
    let big = Rc::new(Style {
        size: small.size * 1.45,
        color: ink,
        ..(*small).clone()
    });
    let mut area = Rect {
        x: rect.x + 6.0,
        y: rect.y + 6.0,
        w: rect.w - 12.0,
        h: rect.h - 12.0,
    };
    if let Some(title) = &chart.title {
        text(
            canvas,
            title,
            rect.x + rect.w / 2.0,
            area.y + big.size,
            &big,
            "middle",
        );
        area.y += big.size * 1.8;
        area.h -= big.size * 1.8;
    }
    let pie = matches!(chart.kind, Kind::Pie { .. });
    let legend_names: Vec<(String, Color)> = if pie {
        chart
            .categories
            .iter()
            .enumerate()
            .map(|(i, c)| (c.clone(), slice_color(&chart.series[0], theme, i)))
            .collect()
    } else {
        chart
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name.clone(), s.color.unwrap_or_else(|| accent(theme, i))))
            .collect()
    };
    if chart.legend && !legend_names.is_empty() {
        let row = small.size * 1.6;
        area.h -= row;
        let widths: Vec<f32> = legend_names
            .iter()
            .map(|(n, _)| typeset::measure(n, &small) + small.size * 1.6)
            .collect();
        let total: f32 = widths.iter().sum();
        let mut x = rect.x + (rect.w - total).max(0.0) / 2.0;
        let y = area.y + area.h + row * 0.75;
        for ((name, color), w) in legend_names.iter().zip(&widths) {
            let _ = write!(
                canvas.body,
                r#"<rect x="{x:.2}" y="{:.2}" width="{:.2}" height="{:.2}" fill="{}"/>"#,
                y - small.size * 0.7,
                small.size * 0.7,
                small.size * 0.7,
                color.css()
            );
            text(canvas, name, x + small.size, y, &small, "start");
            x += w;
        }
    }
    match chart.kind {
        Kind::Pie { hole } => draw_pie(canvas, &chart, area, theme, hole),
        kind => draw_axes(canvas, &chart, area, theme, kind, &small),
    }
}

fn slice_color(series: &Series, theme: &Theme, i: usize) -> Color {
    series
        .points
        .iter()
        .find(|(p, _)| *p == i)
        .map(|(_, c)| *c)
        .unwrap_or_else(|| accent(theme, i))
}

fn draw_pie(canvas: &mut Canvas, chart: &Chart, area: Rect, theme: &Theme, hole: f32) {
    let series = &chart.series[0];
    let total: f64 = series.values.iter().flatten().filter(|v| **v > 0.0).sum();
    if total <= 0.0 {
        return;
    }
    let r = area.w.min(area.h) / 2.0 * 0.9;
    let (cx, cy) = (area.x + area.w / 2.0, area.y + area.h / 2.0);
    let mut angle = -std::f32::consts::FRAC_PI_2;
    for (i, v) in series.values.iter().enumerate() {
        let Some(v) = v.filter(|v| *v > 0.0) else {
            continue;
        };
        let sweep = (v / total) as f32 * std::f32::consts::TAU;
        let end = angle + sweep;
        let large = u8::from(sweep > std::f32::consts::PI);
        let (x1, y1) = (cx + r * angle.cos(), cy + r * angle.sin());
        let (x2, y2) = (cx + r * end.cos(), cy + r * end.sin());
        let d = if sweep >= std::f32::consts::TAU - 0.001 {
            format!(
                "M{:.2} {cy:.2} A{r:.2} {r:.2} 0 1 1 {:.2} {cy:.2} A{r:.2} {r:.2} 0 1 1 {:.2} {cy:.2} Z",
                cx - r,
                cx + r,
                cx - r
            )
        } else {
            format!(
                "M{cx:.2} {cy:.2} L{x1:.2} {y1:.2} A{r:.2} {r:.2} 0 {large} 1 {x2:.2} {y2:.2} Z"
            )
        };
        let _ = write!(
            canvas.body,
            r#"<path d="{d}" fill="{}" stroke="white" stroke-width="1"/>"#,
            slice_color(series, theme, i).css()
        );
        angle = end;
    }
    if hole > 0.0 {
        let paper = theme.scheme("bg1").unwrap_or(Color::WHITE);
        let _ = write!(
            canvas.body,
            r#"<circle cx="{cx:.2}" cy="{cy:.2}" r="{:.2}" fill="{}"/>"#,
            r * hole,
            paper.css()
        );
    }
}

fn draw_axes(
    canvas: &mut Canvas,
    chart: &Chart,
    area: Rect,
    theme: &Theme,
    kind: Kind,
    small: &Rc<Style>,
) {
    let count = chart.categories.len().max(1);
    let stacked = matches!(kind, Kind::Bar { stacked: true, .. });
    let (mut low, mut high) = (0.0f64, 0.0f64);
    for i in 0..count {
        if stacked {
            let (mut pos, mut neg) = (0.0, 0.0);
            for s in &chart.series {
                match s.values.get(i).copied().flatten() {
                    Some(v) if v >= 0.0 => pos += v,
                    Some(v) => neg += v,
                    None => {}
                }
            }
            high = high.max(pos);
            low = low.min(neg);
        } else {
            for s in &chart.series {
                if let Some(v) = s.values.get(i).copied().flatten() {
                    high = high.max(v);
                    low = low.min(v);
                }
            }
        }
    }
    let (min, max, step) = nice_scale(low, high);
    let horizontal = matches!(
        kind,
        Kind::Bar {
            horizontal: true,
            ..
        }
    );
    let label_w = (min..=max)
        .step_by_f64(step)
        .map(|v| typeset::measure(&label(v), small))
        .fold(0.0f32, f32::max);
    let cat_w = chart
        .categories
        .iter()
        .map(|c| typeset::measure(c, small))
        .fold(0.0f32, f32::max);
    let plot = if horizontal {
        Rect {
            x: area.x + cat_w.min(area.w * 0.3) + 6.0,
            y: area.y,
            w: area.w - cat_w.min(area.w * 0.3) - 12.0,
            h: area.h - small.size * 1.8,
        }
    } else {
        Rect {
            x: area.x + label_w + 6.0,
            y: area.y + small.size * 0.5,
            w: area.w - label_w - 10.0,
            h: area.h - small.size * 2.3,
        }
    };
    if plot.w <= 0.0 || plot.h <= 0.0 {
        return;
    }
    let grid = Color {
        alpha: 0.18,
        ..theme.scheme("tx1").unwrap_or(Color::BLACK)
    };
    let span = (max - min).max(1e-9);
    // Value to position along the value axis.
    let at = |v: f64| -> f32 {
        let share = ((v - min) / span) as f32;
        if horizontal {
            plot.x + share * plot.w
        } else {
            plot.y + plot.h - share * plot.h
        }
    };
    for v in (min..=max).step_by_f64(step) {
        let p = at(v);
        if horizontal {
            let _ = write!(
                canvas.body,
                r#"<line x1="{p:.2}" y1="{:.2}" x2="{p:.2}" y2="{:.2}" stroke="{}" stroke-opacity="{:.2}" stroke-width="0.75"/>"#,
                plot.y,
                plot.y + plot.h,
                grid.css(),
                grid.alpha
            );
            text(
                canvas,
                &label(v),
                p,
                plot.y + plot.h + small.size * 1.3,
                small,
                "middle",
            );
        } else {
            let _ = write!(
                canvas.body,
                r#"<line x1="{:.2}" y1="{p:.2}" x2="{:.2}" y2="{p:.2}" stroke="{}" stroke-opacity="{:.2}" stroke-width="0.75"/>"#,
                plot.x,
                plot.x + plot.w,
                grid.css(),
                grid.alpha
            );
            text(
                canvas,
                &label(v),
                plot.x - 4.0,
                p + small.size * 0.35,
                small,
                "end",
            );
        }
    }
    let band = if horizontal { plot.h } else { plot.w } / count as f32;
    let every = ((cat_w + 4.0) / band).ceil().max(1.0) as usize;
    for (i, c) in chart.categories.iter().enumerate() {
        let mid = i as f32 * band + band / 2.0;
        if horizontal {
            text(
                canvas,
                c,
                plot.x - 4.0,
                plot.y + mid + small.size * 0.35,
                small,
                "end",
            );
        } else if i % every == 0 {
            text(
                canvas,
                c,
                plot.x + mid,
                plot.y + plot.h + small.size * 1.3,
                small,
                "middle",
            );
        }
    }
    let colors: Vec<Color> = chart
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| s.color.unwrap_or_else(|| accent(theme, i)))
        .collect();
    let zero = at(0.0f64.clamp(min, max));
    match kind {
        Kind::Bar { stacked, .. } => {
            let n = if stacked {
                1
            } else {
                chart.series.len().max(1)
            };
            let bar = band * 0.66 / n as f32;
            let mut offsets = vec![(0.0f64, 0.0f64); count];
            for (s, series) in chart.series.iter().enumerate() {
                for (i, v) in series.values.iter().enumerate() {
                    let Some(v) = *v else { continue };
                    let (from, to) = if stacked {
                        let o = &mut offsets[i];
                        if v >= 0.0 {
                            let from = o.0;
                            o.0 += v;
                            (from, o.0)
                        } else {
                            let from = o.1;
                            o.1 += v;
                            (from, o.1)
                        }
                    } else {
                        (0.0, v)
                    };
                    let lane = band * 0.17 + if stacked { 0.0 } else { s as f32 * bar };
                    let (a, b) = (at(from.clamp(min, max)), at(to.clamp(min, max)));
                    let (x, y, w, h) = if horizontal {
                        (
                            a.min(b),
                            plot.y + i as f32 * band + lane,
                            (a - b).abs(),
                            bar,
                        )
                    } else {
                        (
                            plot.x + i as f32 * band + lane,
                            a.min(b),
                            bar,
                            (a - b).abs(),
                        )
                    };
                    let _ = write!(
                        canvas.body,
                        r#"<rect x="{x:.2}" y="{y:.2}" width="{w:.2}" height="{h:.2}" fill="{}"/>"#,
                        colors[s].css()
                    );
                }
            }
        }
        Kind::Line | Kind::Area => {
            for (s, series) in chart.series.iter().enumerate() {
                let points: Vec<(f32, f32)> = series
                    .values
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| {
                        Some((
                            plot.x + i as f32 * band + band / 2.0,
                            at(v.as_ref()?.clamp(min, max)),
                        ))
                    })
                    .collect();
                if points.is_empty() {
                    continue;
                }
                let path: Vec<String> = points
                    .iter()
                    .map(|(x, y)| format!("{x:.2},{y:.2}"))
                    .collect();
                if kind == Kind::Area {
                    let first = points[0].0;
                    let last = points[points.len() - 1].0;
                    let _ = write!(
                        canvas.body,
                        r#"<polygon points="{first:.2},{zero:.2} {} {last:.2},{zero:.2}" fill="{}" fill-opacity="0.85"/>"#,
                        path.join(" "),
                        colors[s].css()
                    );
                } else {
                    let _ = write!(
                        canvas.body,
                        r#"<polyline points="{}" fill="none" stroke="{}" stroke-width="2.25" stroke-linejoin="round"/>"#,
                        path.join(" "),
                        colors[s].css()
                    );
                    for (x, y) in &points {
                        let _ = write!(
                            canvas.body,
                            r#"<circle cx="{x:.2}" cy="{y:.2}" r="2.5" fill="{}"/>"#,
                            colors[s].css()
                        );
                    }
                }
            }
        }
        Kind::Pie { .. } => {}
    }
    let axis = Color {
        alpha: 0.45,
        ..theme.scheme("tx1").unwrap_or(Color::BLACK)
    };
    let _ = if horizontal {
        write!(
            canvas.body,
            r#"<line x1="{zero:.2}" y1="{:.2}" x2="{zero:.2}" y2="{:.2}" stroke="{}" stroke-opacity="{:.2}"/>"#,
            plot.y,
            plot.y + plot.h,
            axis.css(),
            axis.alpha
        )
    } else {
        write!(
            canvas.body,
            r#"<line x1="{:.2}" y1="{zero:.2}" x2="{:.2}" y2="{zero:.2}" stroke="{}" stroke-opacity="{:.2}"/>"#,
            plot.x,
            plot.x + plot.w,
            axis.css(),
            axis.alpha
        )
    };
}

trait StepBy {
    fn step_by_f64(self, step: f64) -> Box<dyn Iterator<Item = f64>>;
}

impl StepBy for std::ops::RangeInclusive<f64> {
    fn step_by_f64(self, step: f64) -> Box<dyn Iterator<Item = f64>> {
        let (start, end) = self.into_inner();
        let n = ((end - start) / step).round().max(0.0) as usize;
        Box::new((0..=n.min(50)).map(move |i| start + i as f64 * step))
    }
}

/// The chart as markdown: its title and a table of its values.
pub fn markdown(xml: &str, theme: &Theme) -> String {
    let Some(chart) = parse(xml, theme) else {
        return String::new();
    };
    let mut md = String::new();
    if let Some(title) = &chart.title {
        let _ = write!(md, "**{}**\n\n", md_escape(title));
    }
    let names: Vec<String> = chart.series.iter().map(|s| md_escape(&s.name)).collect();
    let _ = writeln!(md, "| | {} |", names.join(" | "));
    let _ = writeln!(md, "|{}", " --- |".repeat(names.len() + 1));
    for (i, c) in chart.categories.iter().enumerate() {
        let values: Vec<String> = chart
            .series
            .iter()
            .map(|s| {
                s.values
                    .get(i)
                    .copied()
                    .flatten()
                    .map(|v| v.to_string())
                    .unwrap_or_default()
            })
            .collect();
        let _ = writeln!(md, "| {} | {} |", md_escape(c), values.join(" | "));
    }
    md
}

#[cfg(test)]
mod tests {
    use super::*;

    const BAR: &str = r#"<c:chartSpace xmlns:c="c" xmlns:a="a"><c:chart><c:title><c:tx><c:rich><a:p><a:r><a:t>Sales</a:t></a:r></a:p></c:rich></c:tx></c:title><c:plotArea><c:barChart><c:barDir val="col"/><c:grouping val="clustered"/>
      <c:ser><c:tx><c:strRef><c:strCache><c:pt idx="0"><c:v>2025</c:v></c:pt></c:strCache></c:strRef></c:tx>
      <c:cat><c:strRef><c:strCache><c:pt idx="0"><c:v>Q1</c:v></c:pt><c:pt idx="1"><c:v>Q2</c:v></c:pt></c:strCache></c:strRef></c:cat>
      <c:val><c:numRef><c:numCache><c:pt idx="0"><c:v>4.3</c:v></c:pt><c:pt idx="1"><c:v>2.5</c:v></c:pt></c:numCache></c:numRef></c:val></c:ser>
      </c:barChart></c:plotArea><c:legend/></c:chart></c:chartSpace>"#;

    #[test]
    fn reads_cached_series_values() {
        let chart = parse(BAR, &Theme::default()).unwrap();
        assert_eq!(chart.title.as_deref(), Some("Sales"));
        assert_eq!(chart.categories, ["Q1", "Q2"]);
        assert_eq!(chart.series[0].name, "2025");
        assert_eq!(chart.series[0].values, [Some(4.3), Some(2.5)]);
    }

    #[test]
    fn values_read_as_a_table() {
        assert_eq!(
            markdown(BAR, &Theme::default()),
            "**Sales**\n\n| | 2025 |\n| --- | --- |\n| Q1 | 4.3 |\n| Q2 | 2.5 |\n"
        );
    }

    #[test]
    fn scales_are_tidy() {
        assert_eq!(nice_scale(0.0, 4.3), (0.0, 5.0, 1.0));
        assert_eq!(nice_scale(-3.0, 18.0), (-5.0, 20.0, 5.0));
    }

    #[test]
    fn draws_bars() {
        let mut canvas = Canvas::default();
        draw(
            &mut canvas,
            BAR,
            Rect {
                x: 0.0,
                y: 0.0,
                w: 400.0,
                h: 300.0,
            },
            &Theme::default(),
        );
        assert_eq!(canvas.body.matches("<rect").count(), 3);
    }
}
