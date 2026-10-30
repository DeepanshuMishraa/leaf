//! Mermaid diagrams, drawn by Pele (vendored in `vendor/pele`) under
//! whichever JavaScript runtime is installed, then rasterized with resvg.
//! Without a runtime, or when Pele can't read a diagram, it stays a code block.

use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{Arc, LazyLock};

use image::RgbaImage;
use regex::Regex;
use resvg::{tiny_skia, usvg};
use thiserror::Error;

use crate::doc::Doc;
use crate::mono::Rgb;
use crate::render;

/// Diagrams are drawn sharp at up to this many times the size ondine laid them out at.
const MAX_SCALE: u32 = 4;
/// Reject diagrams with unreasonable source dimensions.
const MAX_DIAGRAM_DIMENSION: f64 = 16_384.0;
/// Maximum number of pixels allocated for a rasterized diagram (64 MiB RGBA).
const MAX_RASTER_PIXELS: u64 = 16 * 1024 * 1024;
/// Maximum width or height of a rasterized diagram.
const MAX_RASTER_DIMENSION: f64 = 16_384.0;
/// Maximum depth of CSS variable nesting to resolve before giving up.
const MAX_CSS_VAR_NESTING_DEPTH: usize = 4;
/// Families tried for Pele's `sans-serif`, the first one installed wins.
const SANS: &[&str] = &["Helvetica", "Arial", "Liberation Sans", "DejaVu Sans", "Noto Sans"];
/// Host options for ondine::render — keeps pathological sources bounded.
const RENDER_OPTIONS: &str = r#"{
  "responsive": false,
  "links": false,
  "images": false,
  "limit": 50000,
  "maxEdges": 5000,
  "outputLimit": 4000000
}"#;

pub const DIAGRAM_PREFIX: &str = "mermaid-";

// static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"var\(\s*(--[\w-]+)\s*(?:,\s*([^()]*?))?\s*\)").expect("pattern is valid"));
static DECL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(--[\w-]+)\s*:\s*([^;"]+)"#).expect("pattern is valid"));
static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"style="([^"]*)""#).expect("pattern is valid"));
static CUSTOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"--[\w-]+\s*:[^;]*;?").expect("pattern is valid"));
/*static SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<svg\b[^>]*?\bwidth="([\d.]+)"[^>]*?\bheight="([\d.]+)""#)
        .expect("pattern is valid")
});*/

static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    let installed = |family: &&&str| fonts.faces().any(|face| face.families.iter().any(|(name, _)| name == **family));
    if let Some(family) = SANS.iter().find(installed) {
        fonts.set_sans_serif_family(*family);
    }
    Arc::new(fonts)
});

pub struct Diagram {
    svg: String,
    pub size: (u32, u32),
}

#[derive(Debug, Error, Clone)]
pub enum DrawError {
    #[error("Rendering Engine Failure (ondine/resvg): {0}")]
    RenderError(String),
}

pub type DiagramResult = Result<Diagram, DrawError>;

pub fn diagrams(
    markdown: &str,
    id_for: impl Fn(usize) -> String,
    draw: impl FnOnce(&[String]) -> Vec<DiagramResult>,
) -> (String, Vec<Diagram>) {
    let mut blocks: Vec<(Range<usize>, String)> = Vec::new();
    let mut open: Option<(usize, String)> = None;

    for (event, range) in Parser::new_ext(markdown, render::MARKDOWN).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) if info.split_whitespace().next() == Some("mermaid") => {
                open = Some((range.start, String::new()));
            }
            Event::Text(text) => {
                if let Some((_, source)) = &mut open {
                    source.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((start, source)) = open.take() {
                    blocks.push((start..range.end, source));
                }
            }
            _ => {}
        }
    }

    if blocks.is_empty() {
        return (markdown.to_owned(), Vec::new());
    }

    let candidates: Vec<_> = blocks.into_iter().filter(|(_, source)| looks_like_mermaid(source)).collect();

    if candidates.is_empty() {
        return (markdown.to_owned(), Vec::new());
    }

    let sources: Vec<String> = candidates.iter().map(|(_, s)| s.clone()).collect();
    let drawn_results = draw(&sources);

    let mut found = Vec::new();
    let mut edits = Vec::new();
    for ((range, _), diagram) in candidates.into_iter().zip(drawn_results) {
        let Ok(diagram) = diagram else { continue };
        let id = id_for(found.len());
        edits.push((range, format!("\n![]({})\n", Doc::image_href(&id))));
        found.push(diagram);
    }

    (splice(markdown, edits), found)
}

pub(crate) fn splice(markdown: &str, edits: Vec<(Range<usize>, String)>) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut at = 0;
    for (range, text) in edits {
        if range.start < at {
            continue;
        }
        out.push_str(&markdown[at..range.start]);
        out.push_str(&text);
        at = range.end;
    }
    out.push_str(&markdown[at..]);
    out
}

pub fn is_diagram_id(id: &str) -> bool {
    id.starts_with(DIAGRAM_PREFIX)
}

/// Draws every source, in order; an error for one that can't be drawn.
pub fn render(sources: &[String]) -> Vec<DiagramResult> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    sources
        .iter()
        .map(|src| {
            let drawn = ondine::render(src, RENDER_OPTIONS, now).map_err(|err| DrawError::RenderError(format!("{err:?}")))?;
            Diagram::new(drawn.svg, drawn.width, drawn.height)
                .ok_or_else(|| DrawError::RenderError("Invalid diagram dimensions or another problem happened in `Diagram::new`.".into()))
        })
        .collect()
}

fn hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// `a` moved `amount` of the way to `b`, as a CSS color.
fn mix(a: Rgb, b: Rgb, amount: f32) -> String {
    let amount = amount.clamp(0.0, 1.0);
    hex(std::array::from_fn(|i| {
        (a[i] as f32 + (b[i] as f32 - a[i] as f32) * amount).round() as u8
    }))
}

fn palette(ink: Rgb, paper: Rgb) -> Vec<(&'static str, String)> {
    let mut colors = vec![
        ("--ondine-bg", hex(paper)),
        ("--ondine-fg", hex(ink)),
        ("--ondine-line", hex(ink)),
        ("--ondine-muted", mix(ink, paper, 0.35)),
        ("--ondine-border", mix(ink, paper, 0.5)),
        ("--ondine-surface", mix(paper, ink, 0.08)),
        ("--ondine-surface-alt", mix(paper, ink, 0.16)),
        ("--ondine-accent", hex(ink)),
    ];

    const SERIES: [(&str, u8); 8] = [
        ("--ondine-series-1", 100),
        ("--ondine-series-2", 57),
        ("--ondine-series-3", 86),
        ("--ondine-series-4", 50),
        ("--ondine-series-5", 79),
        ("--ondine-series-6", 64),
        ("--ondine-series-7", 93),
        ("--ondine-series-8", 71),
    ];

    colors.extend(
        SERIES
            .into_iter()
            .map(|(name, foreground)| (name, mix(ink, paper, 1.0 - foreground as f32 / 100.0))),
    );

    colors
}

/// Finds the closing parenthesis matching the opening one at `start`.
fn matching_paren(input: &str, start: usize) -> Option<usize> {
    let mut depth = 0;

    for (offset, byte) in input.as_bytes().iter().enumerate().skip(start) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }

    None
}

/// Resolves CSS `var()` references using known values and fallbacks.
fn replace_vars(svg: &str, known: &HashMap<&str, &str>) -> String {
    let mut out = String::with_capacity(svg.len());
    let mut at = 0;

    while let Some(offset) = svg[at..].find("var(") {
        let start = at + offset;
        let content_start = start + 4;

        let Some(end) = matching_paren(svg, start + 3) else {
            break;
        };

        out.push_str(&svg[at..start]);

        let content = &svg[content_start..end];
        let mut depth = 0;
        let mut comma = None;

        for (i, byte) in content.bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b',' if depth == 0 => {
                    comma = Some(i);
                    break;
                }
                _ => {}
            }
        }

        let (name, fallback) = match comma {
            Some(i) => (content[..i].trim(), Some(content[i + 1..].trim())),
            None => (content.trim(), None),
        };

        let replacement = known.get(name).copied().or(fallback);

        match replacement {
            Some(value) => out.push_str(value),
            None => out.push_str(&svg[start..=end]),
        }

        at = end + 1;
    }

    out.push_str(&svg[at..]);
    out
}

/// Swaps the CSS variables Pele colors with for their values (resvg doesn't
/// read variables). Pele declares its own variables from `--pele-*` ones,
/// with fallbacks for those `set` leaves out. The declarations go once
/// used: resvg drops a style that has any.
fn resolve_vars(svg: &str, set: &[(&str, String)]) -> String {
    let mut svg = svg.to_owned();
    // Each pass resolves one level of variables defined from others.
    for _ in 0..MAX_CSS_VAR_NESTING_DEPTH {
        let mut known: HashMap<&str, &str> = DECL
            .captures_iter(&svg)
            .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str().trim())))
            .filter(|(_, value)| !value.contains("var("))
            .collect();
        known.extend(set.iter().map(|(name, value)| (*name, value.as_str())));
        /*let next = VAR.replace_all(&svg, |c: &regex::Captures| match (known.get(&c[1]), c.get(2)) {
            (Some(value), _) => (*value).to_owned(),
            (None, Some(fallback)) => fallback.as_str().to_owned(),
            (None, None) => c[0].to_owned(),
        });*/
        let next = replace_vars(&svg, &known);
        if next == svg {
            break;
        }
        svg = next;
    }
    STYLE
        .replace_all(&svg, |c: &regex::Captures| format!(r#"style="{}""#, CUSTOM.replace_all(&c[1], "")))
        .into_owned()
}

impl Diagram {
    pub fn new(svg: String, width: f64, height: f64) -> Option<Diagram> {
        if !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || width > MAX_DIAGRAM_DIMENSION
            || height > MAX_DIAGRAM_DIMENSION
        {
            return None;
        }

        let w = width.ceil() as u32;
        let h = height.ceil() as u32;

        Some(Diagram {
            svg: svg.to_owned(),
            size: (w, h),
        })
    }

    /// The diagram in `ink` on a transparent `paper`, at most `width` pixels wide.
    pub fn rasterize(&self, width: u32, ink: Rgb, paper: Rgb) -> Option<RgbaImage> {
        let width = width.min(self.size.0.saturating_mul(MAX_SCALE)).max(1);
        let options = usvg::Options {
            fontdb: FONTS.clone(),
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(&resolve_vars(&self.svg, &palette(ink, paper)), &options).ok()?;
        let tree_width = tree.size().width() as f64;
        let tree_height = tree.size().height() as f64;

        if !tree_width.is_finite() || !tree_height.is_finite() || tree_width <= 0.0 || tree_height <= 0.0 {
            return None;
        }

        let requested_scale = width as f64 / tree_width;
        let dimension_scale = (MAX_RASTER_DIMENSION / tree_width).min(MAX_RASTER_DIMENSION / tree_height);
        let pixel_scale = (MAX_RASTER_PIXELS as f64 / (tree_width * tree_height)).sqrt();

        let scale = requested_scale.min(dimension_scale).min(pixel_scale);
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }

        let raster_width = (tree_width * scale).ceil().min(width as f64).min(MAX_RASTER_DIMENSION) as u32;
        let raster_height = (tree_height * scale).ceil() as u32;

        if raster_width == 0
            || raster_height == 0
            || raster_width > MAX_RASTER_DIMENSION as u32
            || raster_height > MAX_RASTER_DIMENSION as u32
            || (raster_width as u64) * (raster_height as u64) > MAX_RASTER_PIXELS
        {
            return None;
        }

        let mut pixmap = tiny_skia::Pixmap::new(raster_width, raster_height)?;
        //let scale = width.max(1) as f32 / tree.size().width();
        //let height = (tree.size().height() * scale).ceil() as u32;
        //let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
        //resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale as f32, scale as f32),
            &mut pixmap.as_mut(),
        );

        let pixels = pixmap.pixels().iter().flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        });
        RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
    }
}

/// Removes a complete leading YAML or TOML front-matter section.
/// If the section is missing its closing delimiter, leaves the source intact.
fn strip_front_matter(source: &str) -> &str {
    let mut lines = source.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return source;
    };

    let delimiter = first.trim();
    let closing = match delimiter {
        "---" => &["---", "..."][..],
        "+++" => &["+++"][..],
        _ => return source,
    };

    let mut offset = first.len();

    for line in lines {
        let is_closing = closing.contains(&line.trim());
        if is_closing {
            return &source[offset + line.len()..];
        }
        offset += line.len();
    }

    source
}

pub fn looks_like_mermaid(source: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "flowchart",
        "graph",
        "sequenceDiagram",
        "classDiagram",
        "stateDiagram",
        "stateDiagram-v2",
        "erDiagram",
        "gantt",
        "pie",
        "gitGraph",
        "mindmap",
        "timeline",
        "journey",
        "quadrantChart",
        "sankey",
        "xychart",
        "block",
        "C4Context",
        "C4Container",
        "C4Component",
        "requirementDiagram",
        "architecture",
    ];
    let head = strip_front_matter(source)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("%%"))
        .unwrap_or("");
    KEYWORDS.iter().any(|k| {
        head == *k || head.starts_with(&format!("{k} ")) || head.starts_with(&format!("{k}\t")) || head.starts_with(&format!("{k}-"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_stripped_before_mermaid_detection() {
        assert!(looks_like_mermaid(
            "---\ntitle: Example\nconfig:\n  theme: dark\n---\nflowchart TD\n  A --> B"
        ));

        assert!(looks_like_mermaid("+++\ntitle = 'Example'\n+++\nsequenceDiagram\n  A->>B: Hello"));

        assert!(looks_like_mermaid("---\r\ntitle: Example\r\n---\r\nflowchart TD\r\n  A --> B"));

        assert!(!looks_like_mermaid("---\ntitle: Example\n---\nthis is not a diagram"));

        assert!(!looks_like_mermaid("---\ntitle: Example\nflowchart TD"));
    }

    #[test]
    fn diagrams_process_valid_blocks_and_preserve_invalid_and_other_blocks() {
        let md = concat!(
            "```mermaid\n",
            "---\n",
            "title: Example\n",
            "config:\n",
            "  theme: dark\n",
            "---\n",
            "flowchart TD\n",
            "  A --> B\n",
            "```\n",
            "\n",
            "```mermaid\n",
            "this is not a diagram\n",
            "```\n",
            "\n",
            "```rust\n",
            "fn main() {}\n",
            "```\n",
        );

        let mut received = Vec::new();

        let (out, found) = diagrams(
            md,
            |i| format!("test-{i}"),
            |sources| {
                received = sources.to_vec();
                sources
                    .iter()
                    .map(|_| Ok(Diagram::new("<svg/>".to_owned(), 100.0, 100.0).unwrap()))
                    .collect()
            },
        );

        assert_eq!(received.len(), 1);
        assert_eq!(found.len(), 1);

        // The valid diagram is replaced with an image.
        assert!(out.contains("![](leaf-image:test-0)"));

        // Invalid Mermaid remains available as source code.
        assert!(out.contains("```mermaid\nthis is not a diagram\n```"));

        // Non-Mermaid fenced blocks are untouched.
        assert!(out.contains("```rust\nfn main() {}\n```"));
    }

    #[test]
    fn diagram_rejects_invalid_or_excessive_dimensions() {
        assert!(Diagram::new("<svg/>".into(), 0.0, 100.0).is_none());
        assert!(Diagram::new("<svg/>".into(), -1.0, 100.0).is_none());
        assert!(Diagram::new("<svg/>".into(), f64::NAN, 100.0).is_none());
        assert!(Diagram::new("<svg/>".into(), 100.0, f64::INFINITY).is_none());
        assert!(Diagram::new("<svg/>".into(), 100.0, MAX_DIAGRAM_DIMENSION + 1.0).is_none());
    }

    #[test]
    fn diagram_accepts_dimensions_within_limits() {
        let diagram = Diagram::new("<svg/>".into(), MAX_DIAGRAM_DIMENSION, MAX_DIAGRAM_DIMENSION);

        assert!(diagram.is_some());
    }

    #[test]
    fn variables_resolve_nested_color_mix_fallbacks() {
        let svg = r##"<svg fill="var(--ondine-series-1,color-mix(in srgb,var(--_fg) 100%,var(--_bg)))"/>"##;

        assert_eq!(
            resolve_vars(svg, &[("--ondine-series-1", "#eee".into())]),
            r##"<svg fill="#eee"/>"##
        );
    }

    #[test]
    fn variables_resolve_nested_fallbacks() {
        let svg = r##"<svg fill="var(--unknown,var(--known,#fff))"/>"##;

        assert_eq!(resolve_vars(svg, &[("--known", "#eee".into())]), r##"<svg fill="#eee"/>"##);
    }

    #[test]
    fn variables_resolve_through_their_fallbacks() {
        let svg = r##"<svg style="--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);" fill="var(--_fg)"><rect fill="var(--_bg)" stroke="var(--unknown)"/></svg>"##;
        assert_eq!(
            resolve_vars(svg, &[]),
            r##"<svg style="" fill="#1f1f1f"><rect fill="#fff" stroke="var(--unknown)"/></svg>"##
        );
        assert_eq!(
            resolve_vars(svg, &[("--pele-fg", "#eee".into())]),
            r##"<svg style="" fill="#eee"><rect fill="#fff" stroke="var(--unknown)"/></svg>"##
        );
    }

    #[test]
    fn colors_mix_toward_each_other() {
        assert_eq!(mix([0, 0, 0], [255, 255, 255], 0.5), "#808080");
        assert_eq!(hex([10, 20, 30]), "#0a141e");
    }
}
