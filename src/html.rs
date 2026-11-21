//! HTML pages saved on disk, plain or bundled into one file, read as markdown.
//! The page's text, links, tables and pictures show in the terminal's own font
//! and colors; its stylesheets and scripts are left out.

use std::path::Path;
use std::sync::LazyLock;

use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, Options};
use regex::Regex;

use crate::doc::Doc;
use crate::markdown;

const EXTENSIONS: [&str; 4] = ["html", "htm", "xhtml", "xht"];

/// Tags whose contents aren't text to read: page settings, styling, code and
/// drawings that only show in a browser.
const SKIPPED: [&str; 9] = [
    "head", "script", "style", "noscript", "template", "iframe", "svg", "canvas", "object",
];

static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title>").expect("pattern is valid"));

#[derive(Debug, thiserror::Error)]
pub enum HtmlFailed {
    #[error(
        "Could not read {path} because the operating system denied access ({cause}). Allow your terminal to read it in your system's privacy settings, then restart it."
    )]
    Blocked { path: String, cause: std::io::Error },
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
    #[error(
        "Found no text to show in {path}. The page may build its content with JavaScript, which leaf doesn't run. Open it in a browser instead."
    )]
    Empty { path: String },
}

pub fn is_html(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

pub fn open(path: &Path) -> Result<Doc, HtmlFailed> {
    let shown = || path.display().to_string();
    let bytes = std::fs::read(path).map_err(|cause| {
        if cause.kind() == std::io::ErrorKind::PermissionDenied {
            HtmlFailed::Blocked {
                path: shown(),
                cause,
            }
        } else {
            HtmlFailed::Read {
                path: shown(),
                cause,
            }
        }
    })?;
    let html = String::from_utf8_lossy(&bytes);
    let markdown = to_markdown(&html).ok_or_else(|| HtmlFailed::Empty { path: shown() })?;
    let title = title(&html).unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    Ok(markdown::from_converted(title, &markdown, path))
}

/// The page as markdown, or `None` when it holds no text.
fn to_markdown(html: &str) -> Option<String> {
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Options::default()
        })
        .skip_tags(SKIPPED.to_vec())
        .build();
    let markdown = converter.convert(html).ok()?.trim().to_owned();
    (!markdown.is_empty()).then_some(markdown)
}

/// The text of the page's `<title>`, with its common entities spelled out.
fn title(html: &str) -> Option<String> {
    let raw = TITLE.captures(html)?.get(1)?.as_str();
    let text = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_reads_as_markdown_without_its_styles_or_scripts() {
        let html = r#"<!doctype html><html><head><title>My &amp; Page</title>
            <style>h1 { color: red }</style></head>
            <body><h1>Hello</h1><p>Some <b>bold</b> text and <a href="b.html">a link</a>.</p>
            <script>alert("hi")</script><ul><li>one</li><li>two</li></ul></body></html>"#;
        let md = to_markdown(html).expect("has text");
        assert!(md.starts_with("# Hello"), "{md}");
        assert!(
            md.contains("Some **bold** text and [a link](b.html)."),
            "{md}"
        );
        assert!(md.contains("- one\n- two"), "{md}");
        assert!(!md.contains("color") && !md.contains("alert"), "{md}");
        assert_eq!(title(html).as_deref(), Some("My & Page"));
    }

    #[test]
    fn a_page_with_no_text_is_an_error() {
        let dir = std::env::temp_dir().join(format!("leaf-html-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("app.html");
        std::fs::write(
            &path,
            "<html><body><div id=app></div><script>x()</script></body></html>",
        )
        .unwrap();
        assert!(matches!(open(&path), Err(HtmlFailed::Empty { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pictures_beside_the_page_and_inside_it_show() {
        let dir = std::env::temp_dir().join(format!("leaf-html-pics-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        image::RgbaImage::from_pixel(30, 10, image::Rgba([0, 0, 255, 255]))
            .save(dir.join("a.png"))
            .unwrap();
        let svg = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='8' height='4'%3E%3C/svg%3E";
        let path = dir.join("page.html");
        std::fs::write(
            &path,
            format!(r#"<html><body><p>Look</p><img src="a.png"><img src="{svg}"></body></html>"#),
        )
        .unwrap();
        let doc = open(&path).unwrap();
        assert_eq!(doc.title, "page");
        let page = doc.source.page(0);
        assert!(
            page.contains("![](leaf-image:0)") && page.contains("![](leaf-image:1)"),
            "{page}"
        );
        assert_eq!(doc.source.image_size("0"), Some((30, 10)));
        assert_eq!(doc.source.image_size("1"), Some((8, 4)));
        assert!(doc.source.decode("1", 100).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recognizes_html_extensions_in_any_case() {
        assert!(is_html(Path::new("site/Index.HTML")));
        assert!(is_html(Path::new("a.htm")));
        assert!(!is_html(Path::new("a.md")));
    }
}
