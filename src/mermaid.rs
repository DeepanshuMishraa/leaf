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

use crate::doc::Doc;
use crate::mono::Rgb;
use crate::render;

/// Diagrams are drawn sharp at up to this many times the size ondine laid them out at.
const MAX_SCALE: u32 = 4;
/// Families tried for Pele's `sans-serif`, the first one installed wins.
const SANS: &[&str] = &[
    "Helvetica",
    "Arial",
    "Liberation Sans",
    "DejaVu Sans",
    "Noto Sans",
];
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

static VAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"var\(\s*(--[\w-]+)\s*(?:,\s*([^()]*?))?\s*\)").expect("pattern is valid")
});
static DECL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(--[\w-]+)\s*:\s*([^;"]+)"#).expect("pattern is valid"));
static STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"style="([^"]*)""#).expect("pattern is valid"));
static CUSTOM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"--[\w-]+\s*:[^;]*;?").expect("pattern is valid"));
/*static SIZE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<svg\b[^>]*?\bwidth="([\d.]+)"[^>]*?\bheight="([\d.]+)""#)
        .expect("pattern is valid")
});*/

static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_system_fonts();
    let installed = |family: &&&str| {
        fonts
            .faces()
            .any(|face| face.families.iter().any(|(name, _)| name == **family))
    };
    if let Some(family) = SANS.iter().find(installed) {
        fonts.set_sans_serif_family(*family);
    }
    Arc::new(fonts)
});

pub struct Diagram {
    svg: String,
    /// Pixel size Pele laid the diagram out at.
    pub size: (u32, u32),
}

pub fn diagrams(
    markdown: &str,
    id_for: impl Fn(usize) -> String,
    draw: impl FnOnce(&[String]) -> Vec<Option<Diagram>>,
) -> (String, Vec<Diagram>) {
    let mut blocks: Vec<(Range<usize>, String)> = Vec::new();
    let mut open: Option<(Range<usize>, String)> = None;

    for (event, range) in Parser::new_ext(markdown, render::MARKDOWN).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info.split_whitespace().next() == Some("mermaid") =>
            {
                open = Some((range, String::new()));
            }
            Event::Text(text) => {
                if let Some((_, source)) = &mut open {
                    source.push_str(&text);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(block) = open.take() {
                    blocks.push(block);
                }
            }
            _ => {}
        }
    }

    if blocks.is_empty() {
        return (markdown.to_owned(), Vec::new());
    }

    let sources: Vec<String> = blocks.iter().map(|(_, s)| s.clone()).collect();
    let drawn = draw(&sources);

    let mut found = Vec::new();
    let mut edits = Vec::new();
    for ((range, _), diagram) in blocks.into_iter().zip(drawn) {
        let Some(diagram) = diagram else { continue };
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

/// Draws every source, in order; `None` for one that can't be drawn.
pub fn render(sources: &[String]) -> Vec<Option<Diagram>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    sources
        .iter()
        .map(|src| {
            let drawn = ondine::render(src, RENDER_OPTIONS, now).ok()?;
            Diagram::new(drawn.svg, drawn.width, drawn.height)
        })
        .collect()
}

fn hex([r, g, b]: Rgb) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// `a` moved `amount` of the way to `b`, as a CSS color.
fn mix(a: Rgb, b: Rgb, amount: f32) -> String {
    hex([0, 1, 2].map(|i| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * amount).round() as u8))
}

/// Pele's colors for a diagram in `ink` on `paper`: lines and text in the
/// ink, boxes a faint tint of it, borders and notes in between.
fn palette(ink: Rgb, paper: Rgb) -> Vec<(&'static str, String)> {
    vec![
        ("--ondine-bg", hex(paper)),
        ("--ondine-fg", hex(ink)),
        ("--ondine-line", hex(ink)),
        ("--ondine-muted", mix(ink, paper, 0.35)),
        ("--ondine-border", mix(ink, paper, 0.5)),
        ("--ondine-surface", mix(paper, ink, 0.08)),
        ("--ondine-surface-alt", mix(paper, ink, 0.16)),
        ("--ondine-accent", hex(ink)),
    ]
}

/// Swaps the CSS variables Pele colors with for their values (resvg doesn't
/// read variables). Pele declares its own variables from `--pele-*` ones,
/// with fallbacks for those `set` leaves out. The declarations go once
/// used: resvg drops a style that has any.
fn resolve_vars(svg: &str, set: &[(&str, String)]) -> String {
    let mut svg = svg.to_owned();
    // Each pass resolves one level of variables defined from others.
    for _ in 0..4 {
        let mut known: HashMap<&str, &str> = DECL
            .captures_iter(&svg)
            .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str().trim())))
            .filter(|(_, value)| !value.contains("var("))
            .collect();
        known.extend(set.iter().map(|(name, value)| (*name, value.as_str())));
        let next = VAR.replace_all(&svg, |c: &regex::Captures| {
            match (known.get(&c[1]), c.get(2)) {
                (Some(value), _) => (*value).to_owned(),
                (None, Some(fallback)) => fallback.as_str().to_owned(),
                (None, None) => c[0].to_owned(),
            }
        });
        if next == svg {
            break;
        }
        svg = next.into_owned();
    }
    STYLE
        .replace_all(&svg, |c: &regex::Captures| {
            format!(r#"style="{}""#, CUSTOM.replace_all(&c[1], ""))
        })
        .into_owned()
}

impl Diagram {
    pub fn new(svg: String, width: f64, height: f64) -> Option<Diagram> {
        let w = width.ceil().max(1.0) as u32;
        let h = height.ceil().max(1.0) as u32;
        if w == 0 || h == 0 {
            return None;
        }
        Some(Diagram {
            svg: svg.to_owned(),
            size: (w, h),
        })
    }

    /// The diagram in `ink` on a transparent `paper`, at most `width` pixels wide.
    pub fn rasterize(&self, width: u32, ink: Rgb, paper: Rgb) -> Option<RgbaImage> {
        let width = width.min(self.size.0 * MAX_SCALE);
        let options = usvg::Options {
            fontdb: FONTS.clone(),
            ..Default::default()
        };
        let tree =
            usvg::Tree::from_str(&resolve_vars(&self.svg, &palette(ink, paper)), &options).ok()?;
        let scale = width.max(1) as f32 / tree.size().width();
        let height = (tree.size().height() * scale).ceil() as u32;
        let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let pixels = pixmap.pixels().iter().flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        });
        RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
    }
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
    let head = source
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("%%") && !l.starts_with("---"))
        .unwrap_or("");
    KEYWORDS.iter().any(|k| {
        head == *k
            || head.starts_with(&format!("{k} "))
            || head.starts_with(&format!("{k}\t"))
            || head.starts_with(&format!("{k}-"))
    })
}

#[cfg(test)]
mod tests {
    //use std::time::Instant;

    use super::*;

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

    /*
    #[test]
    fn a_runtime_past_its_deadline_is_stopped() {
        let child = Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh runs");
        let start = Instant::now();
        assert_eq!(finish(child, "", Duration::from_millis(100)), None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }
    */
    /*
    #[test]
    fn a_runtime_that_finishes_returns_its_output() {
        let child = Command::new("sh")
            .args(["-c", "cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh runs");
        assert_eq!(finish(child, "svg", DEADLINE).as_deref(), Some("svg"));
    }
    */
    /*
    #[test]
    fn sources_become_js_strings() {
        assert_eq!(js_string("a\"b\\c\nd"), r#""a\"b\\c\u000ad""#);
    }
    */
}
