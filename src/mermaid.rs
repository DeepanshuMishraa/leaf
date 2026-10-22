//! Mermaid diagrams, drawn by Pele (vendored in `vendor/pele`) under
//! whichever JavaScript runtime is installed, then rasterized with resvg.
//! Without a runtime, or when Pele can't read a diagram, it stays a code block.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, LazyLock, mpsc};
use std::time::Duration;

use image::RgbaImage;
use regex::Regex;
use resvg::{tiny_skia, usvg};

use crate::mono::Rgb;

const PELE: &str = include_str!("../vendor/pele/pele.global.js");
/// Draws each source in `SOURCES`, printing the SVGs split by NULs; one Pele can't read is empty.
const DRIVER: &str = r#";process.stdout.write(SOURCES.map(s => { try { return pele.render(s).svg; } catch { return ""; } }).join("\0"));"#;
/// Runtimes tried in turn until one is installed, each reading the script from stdin.
const RUNTIMES: &[&str] = &["bun", "node"];
/// Longest the runtime may take over a document's diagrams; Pele sets no time limit of its own.
const DEADLINE: Duration = Duration::from_secs(10);
/// Diagrams are drawn sharp at up to this many times the size Pele laid them out at.
const MAX_SCALE: u32 = 4;
/// Families tried for Pele's `sans-serif`, the first one installed wins.
const SANS: &[&str] = &["Helvetica", "Arial", "Liberation Sans", "DejaVu Sans", "Noto Sans"];

static VAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"var\(\s*(--[\w-]+)\s*(?:,\s*([^()]*?))?\s*\)").expect("pattern is valid"));
static DECL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(--[\w-]+)\s*:\s*([^;"]+)"#).expect("pattern is valid"));
static STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"style="([^"]*)""#).expect("pattern is valid"));
static CUSTOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"--[\w-]+\s*:[^;]*;?").expect("pattern is valid"));
static SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"<svg\b[^>]*?\bwidth="([\d.]+)"[^>]*?\bheight="([\d.]+)""#).expect("pattern is valid"));

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
    /// Pixel size Pele laid the diagram out at.
    pub size: (u32, u32),
}

/// Draws every source, in order; `None` for one that can't be drawn.
pub fn render(sources: &[String]) -> Vec<Option<Diagram>> {
    let none = || sources.iter().map(|_| None).collect();
    if sources.is_empty() {
        return Vec::new();
    }
    let script = format!("const SOURCES = [{}];\n{PELE}\n{DRIVER}", sources.iter().map(|s| js_string(s)).collect::<Vec<_>>().join(","));
    let spawn = |runtime: &&str| Command::new(runtime).arg("-").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok();
    let Some(child) = RUNTIMES.iter().find_map(spawn) else { return none() };
    let Some(out) = finish(child, &script, DEADLINE) else { return none() };
    let svgs: Vec<&str> = out.split('\0').collect();
    if svgs.len() != sources.len() {
        return none();
    }
    svgs.into_iter().map(Diagram::new).collect()
}

/// Feeds `script` to the runtime and returns what it printed, or `None` if
/// it fails or is still running at `deadline` (then it's killed).
fn finish(mut child: Child, script: &str, deadline: Duration) -> Option<String> {
    let (mut stdin, mut stdout) = (child.stdin.take()?, child.stdout.take()?);
    // Written and read on their own threads, so a runtime that stalls on
    // either pipe still stops at the deadline. A failed write shows as a
    // failed run or a short answer.
    let script = script.to_owned();
    std::thread::spawn(move || stdin.write_all(script.as_bytes()));
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = tx.send(stdout.read_to_end(&mut out).map(|_| out));
    });
    let Ok(Ok(out)) = rx.recv_timeout(deadline) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    child.wait().ok()?.success().then(|| String::from_utf8_lossy(&out).into_owned())
}

/// `text` as a JavaScript string literal.
fn js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
        ("--pele-bg", hex(paper)),
        ("--pele-fg", hex(ink)),
        ("--pele-line", hex(ink)),
        ("--pele-muted", mix(ink, paper, 0.35)),
        ("--pele-border", mix(ink, paper, 0.5)),
        ("--pele-surface", mix(paper, ink, 0.08)),
        ("--pele-surface-alt", mix(paper, ink, 0.16)),
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
        let next = VAR.replace_all(&svg, |c: &regex::Captures| match (known.get(&c[1]), c.get(2)) {
            (Some(value), _) => (*value).to_owned(),
            (None, Some(fallback)) => fallback.as_str().to_owned(),
            (None, None) => c[0].to_owned(),
        });
        if next == svg {
            break;
        }
        svg = next.into_owned();
    }
    STYLE.replace_all(&svg, |c: &regex::Captures| format!(r#"style="{}""#, CUSTOM.replace_all(&c[1], ""))).into_owned()
}

impl Diagram {
    pub fn new(svg: &str) -> Option<Diagram> {
        let size = SIZE.captures(svg)?;
        let size = (size[1].parse::<f32>().ok()?.ceil() as u32, size[2].parse::<f32>().ok()?.ceil() as u32);
        Some(Diagram { svg: svg.to_owned(), size })
    }

    /// The diagram in `ink` on a transparent `paper`, at most `width` pixels wide.
    pub fn rasterize(&self, width: u32, ink: Rgb, paper: Rgb) -> Option<RgbaImage> {
        let width = width.min(self.size.0 * MAX_SCALE);
        let options = usvg::Options { fontdb: FONTS.clone(), ..Default::default() };
        let tree = usvg::Tree::from_str(&resolve_vars(&self.svg, &palette(ink, paper)), &options).ok()?;
        let scale = width.max(1) as f32 / tree.size().width();
        let height = (tree.size().height() * scale).ceil() as u32;
        let mut pixmap = tiny_skia::Pixmap::new(width.max(1), height.max(1))?;
        resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        let pixels = pixmap.pixels().iter().flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        });
        RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixels.collect())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn variables_resolve_through_their_fallbacks() {
        let svg = r##"<svg style="--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);" fill="var(--_fg)"><rect fill="var(--_bg)" stroke="var(--unknown)"/></svg>"##;
        assert_eq!(resolve_vars(svg, &[]), r##"<svg style="" fill="#1f1f1f"><rect fill="#fff" stroke="var(--unknown)"/></svg>"##);
        assert_eq!(resolve_vars(svg, &[("--pele-fg", "#eee".into())]), r##"<svg style="" fill="#eee"><rect fill="#fff" stroke="var(--unknown)"/></svg>"##);
    }

    #[test]
    fn colors_mix_toward_each_other() {
        assert_eq!(mix([0, 0, 0], [255, 255, 255], 0.5), "#808080");
        assert_eq!(hex([10, 20, 30]), "#0a141e");
    }

    #[test]
    fn a_runtime_past_its_deadline_is_stopped() {
        let child = Command::new("sh").args(["-c", "sleep 5"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("sh runs");
        let start = Instant::now();
        assert_eq!(finish(child, "", Duration::from_millis(100)), None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_runtime_that_finishes_returns_its_output() {
        let child = Command::new("sh").args(["-c", "cat"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("sh runs");
        assert_eq!(finish(child, "svg", DEADLINE).as_deref(), Some("svg"));
    }

    #[test]
    fn sources_become_js_strings() {
        assert_eq!(js_string("a\"b\\c\nd"), r#""a\"b\\c\u000ad""#);
    }
}

