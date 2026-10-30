//! Markdown files and links, read as written. Images beside the file (or
//! relative to the link) show inline, and so do Mermaid diagrams.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

use image::RgbaImage;
use pulldown_cmark::{Event, Parser, Tag};
use regex::Regex;
use url::Url;

use crate::doc::{Doc, Source};
use crate::mermaid::{self, DIAGRAM_PREFIX, Diagram, diagrams, splice};
use crate::mono::Rgb;
use crate::render;

#[derive(Debug, thiserror::Error)]
pub enum MarkdownFailed {
    #[error(
        "macOS blocked access to {path} ({cause}). Allow your terminal in System Settings → Privacy & Security → Files & Folders, then restart it."
    )]
    Blocked { path: String, cause: std::io::Error },
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
}

/// YAML (`---`) or TOML (`+++`) front matter: settings for site generators, not text to read.
static FRONT_MATTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:---\r?\n(?s:.*?)\r?\n(?:---|\.\.\.)|\+\+\+\r?\n(?s:.*?)\r?\n\+\+\+)[ \t]*(?:\r?\n|\z)").expect("pattern is valid")
});
static IMG_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(<img\b[^>]*?\bsrc\s*=\s*)(["'])([^"']*)(["'])"#).expect("pattern is valid"));

/// Where a document's relative image paths start from.
#[derive(Clone, Copy)]
enum Base<'a> {
    Dir(&'a Path),
    Url(&'a Url),
}

/// Whether `href` is relative to the document rather than a full address.
fn is_relative(href: &str) -> bool {
    !href.is_empty() && !href.contains("://") && !href.starts_with("data:") && !href.starts_with("mailto:") && !href.starts_with('#')
}

/// Points relative images where the viewer finds them: files beside a local
/// document become embedded ids the source decodes, and paths in a linked
/// document become full URLs. Returns the markdown and each id's file.
fn localize(markdown: &str, base: Base) -> (String, Vec<PathBuf>) {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut id = |href: &str| match base {
        Base::Dir(dir) => {
            let path = href.split(['?', '#']).next().unwrap_or(href).replace("%20", " ");
            files.push(dir.join(path));
            Doc::image_href(&(files.len() - 1).to_string())
        }
        Base::Url(url) => url.join(href).map(String::from).unwrap_or_else(|_| href.to_owned()),
    };
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for (event, range) in Parser::new_ext(markdown, render::MARKDOWN).into_offset_iter() {
        match event {
            Event::Start(Tag::Image { dest_url, .. }) if is_relative(&dest_url) => edits.push((range, format!("![]({})", id(&dest_url)))),
            Event::Html(html) | Event::InlineHtml(html) if IMG_SRC.is_match(&html) => {
                let replaced = IMG_SRC.replace_all(&html, |c: &regex::Captures| {
                    let src = if is_relative(&c[3]) { id(&c[3]) } else { c[3].to_owned() };
                    format!("{}{}{src}{}", &c[1], &c[2], &c[4])
                });
                edits.push((range, replaced.into_owned()));
            }
            _ => {}
        }
    }
    (splice(markdown, edits), files)
}

struct MarkdownSource {
    text: Rc<str>,
    images: Vec<PathBuf>,
    diagrams: Vec<Diagram>,
    /// Pixel sizes read from file headers, asked for on every layout.
    sizes: RefCell<HashMap<usize, Option<(u32, u32)>>>,
}

impl MarkdownSource {
    fn path(&self, id: &str) -> Option<(usize, &Path)> {
        let index: usize = id.parse().ok()?;
        Some((index, self.images.get(index)?))
    }

    fn diagram(&self, id: &str) -> Option<&Diagram> {
        self.diagrams.get(id.strip_prefix(DIAGRAM_PREFIX)?.parse::<usize>().ok()?)
    }
}

impl Source for MarkdownSource {
    fn page(&self, _index: usize) -> Rc<str> {
        self.text.clone()
    }

    // An image that's missing or in a format leaf can't decode (SVG) is left
    // out; the text around it still shows.
    fn image_size(&self, id: &str) -> Option<(u32, u32)> {
        if let Some(diagram) = self.diagram(id) {
            return Some(diagram.size);
        }
        let (index, path) = self.path(id)?;
        *self
            .sizes
            .borrow_mut()
            .entry(index)
            .or_insert_with(|| image::image_dimensions(path).ok())
    }

    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage> {
        let image = image::open(self.path(id)?.1).ok()?;
        let image = if image.width() > width.max(1) {
            image.resize(width.max(1), u32::MAX, image::imageops::FilterType::Triangle)
        } else {
            image
        };
        Some(image.to_rgba8())
    }

    fn is_diagram(&self, id: &str) -> bool {
        self.diagram(id).is_some()
    }

    fn draw(&self, id: &str, width: u32, ink: Rgb, paper: Rgb) -> Option<RgbaImage> {
        self.diagram(id)?.rasterize(width, ink, paper)
    }
}

pub fn open(path: &Path) -> Result<Doc, MarkdownFailed> {
    let bytes = std::fs::read(path).map_err(|cause| {
        let path = path.display().to_string();
        if cause.kind() == std::io::ErrorKind::PermissionDenied {
            MarkdownFailed::Blocked { path, cause }
        } else {
            MarkdownFailed::Read { path, cause }
        }
    })?;
    let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(document(
        title,
        &String::from_utf8_lossy(&bytes),
        Base::Dir(path.parent().unwrap_or(Path::new("."))),
    ))
}

/// A markdown document downloaded from `url`.
pub fn from_url(title: String, text: &str, url: &Url) -> Doc {
    document(title, text, Base::Url(url))
}

fn document(title: String, text: &str, base: Base) -> Doc {
    let (markdown, images) = localize(&FRONT_MATTER.replace(text, ""), base);
    let (markdown, diagrams) = diagrams(&markdown, |i| format!("{}{i}", DIAGRAM_PREFIX), mermaid::render);
    Doc {
        title,
        page_count: None,
        source: Box::new(MarkdownSource {
            text: markdown.into(),
            images,
            diagrams,
            sizes: RefCell::default(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_images_become_embedded_and_web_images_stay() {
        let md = "![logo](<img/a b.png> \"t\") ![](https://x.org/b.png)\n\n<p align=\"center\"><img width=\"9\" src=\"c.png\"></p>\n";
        let (out, files) = localize(md, Base::Dir(Path::new("/docs")));
        assert_eq!(
            out,
            "![](leaf-image:0) ![](https://x.org/b.png)\n\n<p align=\"center\"><img width=\"9\" src=\"leaf-image:1\"></p>\n"
        );
        assert_eq!(files, vec![PathBuf::from("/docs/img/a b.png"), PathBuf::from("/docs/c.png")]);
    }

    #[test]
    fn local_hrefs_drop_queries_and_decode_spaces() {
        let (_, files) = localize("![](my%20pic.png?raw=true)", Base::Dir(Path::new("/d")));
        assert_eq!(files, vec![PathBuf::from("/d/my pic.png")]);
    }

    #[test]
    fn images_in_a_linked_document_resolve_against_its_url() {
        let url = Url::parse("https://raw.githubusercontent.com/o/r/main/docs/README.md").expect("valid url");
        let (out, files) = localize("![](img/a.png) <img src=\"../b.png\">", Base::Url(&url));
        assert_eq!(
            out,
            "![](https://raw.githubusercontent.com/o/r/main/docs/img/a.png) <img src=\"https://raw.githubusercontent.com/o/r/main/b.png\">"
        );
        assert!(files.is_empty());
    }

    /// Runs `examples/mermaid.md` through the real runtime and rasterizer.
    /// Skipped when neither bun nor node is installed.
    #[test]
    fn sample_diagrams_draw_and_rasterize() {
        let md = include_str!("../examples/mermaid.md");
        let (out, found) = mermaid::diagrams(md, |i| format!("{i}"), mermaid::render);
        // 10 mermaid blocks; only the invalid one stays as code.
        assert_eq!(found.len(), 9, "markdown after drawing:\n{out}");
        assert!(out.contains("```mermaid\nthis is not a diagram at all\n```"));
        assert!(out.contains("```rust\nfn main() {"));
        assert!(
            out.contains("\n\nThis line must still show"),
            "text after a diagram merged into it:\n{out}"
        );
        let (ink, paper) = ([230, 230, 230], [20, 20, 20]);
        for (i, diagram) in found.iter().enumerate() {
            eprintln!("Diagram {i}: {:?}", diagram.size);
            assert!(diagram.size.0 > 0 && diagram.size.1 > 0, "diagram {i} has no size");
            let image = diagram
                .rasterize(800, ink, paper)
                .unwrap_or_else(|| panic!("diagram {i} did not rasterize"));
            assert!(image.width() <= 800);
            let inked = image
                .pixels()
                .filter(|p| p[3] > 128 && p[0].abs_diff(ink[0]) < 20 && p[1].abs_diff(ink[1]) < 20 && p[2].abs_diff(ink[2]) < 20)
                .count();
            assert!(inked > 50, "diagram {i} has almost no ink-colored pixels ({inked})");
        }
    }

    #[test]
    fn front_matter_is_not_shown() {
        assert_eq!(FRONT_MATTER.replace("---\ntitle: x\n---\n# Hi", ""), "# Hi");
        assert_eq!(FRONT_MATTER.replace("+++\ntitle = 1\n+++\nBody", ""), "Body");
        assert_eq!(FRONT_MATTER.replace("Text\n\n---\nmore\n---\n", ""), "Text\n\n---\nmore\n---\n");
    }
}
