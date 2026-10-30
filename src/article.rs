//! Web pages as readable articles; links that serve a PDF or markdown open as such.

use std::sync::LazyLock;

use htmd::HtmlToMarkdown;
use htmd::options::{BulletListMarker, Options};
use regex::Regex;
use ureq::ResponseExt;
use url::Url;

use crate::doc::Doc;
use crate::{markdown, pdf};

#[derive(Debug, thiserror::Error)]
pub enum ArticleFailed {
    #[error("Could not reach {url} ({cause}). Check your connection and the URL, then try again.")]
    Fetch { url: String, cause: String },
    #[error("{url} returned HTTP {status}. The page may have moved, need a login or block automated requests.")]
    Http { url: String, status: u16 },
    #[error("The connection to {url} dropped while downloading ({cause}). Try again.")]
    Download { url: String, cause: String },
    #[error(
        "Found no article text at {url}. The page may build its content with JavaScript or sit behind a login. Try opening it in a browser."
    )]
    NotReadable { url: String },
    #[error(transparent)]
    Pdf(#[from] pdf::PdfOpenFailed),
}

/// PDFs from the web can be large books; HTML pages never come close.
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// Heading permalinks ("#", "¶", "🔗") are page chrome, not content.
static PERMALINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\s*[#¶🔗]?\s*\]\([^)]*\)").expect("pattern is valid"));
/// Removing permalinks leaves trailing spaces on headings.
static HEADING_TRAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^(#{1,6} .*?)[ \t]+$").expect("pattern is valid"));

/// Abstract pages hold only the summary; point them at the full paper.
pub fn paper_url(link: &str) -> String {
    match Url::parse(link) {
        Ok(mut url) if url.host_str() == Some("arxiv.org") && url.path().starts_with("/abs/") => {
            let path = url.path().replacen("/abs/", "/pdf/", 1);
            url.set_path(&path);
            url.to_string()
        }
        _ => link.to_owned(),
    }
}

/// A GitHub page showing a markdown file is mostly site chrome; read the file itself.
pub fn raw_markdown_url(link: &str) -> String {
    let Ok(url) = Url::parse(link) else { return link.to_owned() };
    let segments: Vec<&str> = url.path_segments().map(Iterator::collect).unwrap_or_default();
    match (url.host_str(), segments.as_slice()) {
        (Some("github.com"), [owner, repo, "blob", rest @ ..]) if is_markdown_path(url.path()) && !rest.is_empty() => {
            format!("https://raw.githubusercontent.com/{owner}/{repo}/{}", rest.join("/"))
        }
        _ => link.to_owned(),
    }
}

fn is_markdown_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".md") || path.ends_with(".markdown")
}

/// A title for a document without one: the file name from its URL, or the host.
fn link_title(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else { return url.to_owned() };
    let last = parsed.path_segments().and_then(|mut s| s.rfind(|p| !p.is_empty())).unwrap_or("");
    let decoded = percent_decode(last);
    let lower = decoded.to_ascii_lowercase();
    let name = [".pdf", ".md", ".markdown"]
        .iter()
        .find(|ext| lower.ends_with(*ext))
        .map_or(decoded.as_str(), |ext| &decoded[..decoded.len() - ext.len()]);
    if name.is_empty() {
        parsed.host_str().unwrap_or(url).to_owned()
    } else {
        name.to_owned()
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn media_type(content_type: Option<&str>) -> Option<String> {
    content_type
        .and_then(|t| t.split(';').next())
        .map(|t| t.trim().to_ascii_lowercase())
}

fn is_pdf(content_type: Option<&str>, url: &str) -> bool {
    match media_type(content_type).as_deref() {
        Some("application/pdf") => true,
        // Some servers label PDFs as generic binary; fall back to the extension.
        Some("application/octet-stream") => Url::parse(url).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".pdf")),
        _ => false,
    }
}

fn is_markdown(content_type: Option<&str>, url: &str) -> bool {
    match media_type(content_type).as_deref() {
        Some("text/markdown" | "text/x-markdown") => true,
        // Raw file hosts serve markdown as plain text or binary; trust the extension.
        Some("text/plain" | "application/octet-stream") | None => Url::parse(url).is_ok_and(|u| is_markdown_path(u.path())),
        _ => false,
    }
}

/// Converts article HTML to markdown, with links and images made absolute.
/// Exposed separately from `open` so it can be tested without a network.
pub fn from_html(html: &str, url: &str) -> Result<(String, String), ArticleFailed> {
    let not_readable = || ArticleFailed::NotReadable { url: url.to_owned() };
    let mut readability = dom_smoothie::Readability::new(html, Some(url), None).map_err(|_| not_readable())?;
    let article = readability.parse().map_err(|_| not_readable())?;
    if article.content.trim().is_empty() {
        return Err(not_readable());
    }
    let converter = HtmlToMarkdown::builder()
        .options(Options {
            bullet_list_marker: BulletListMarker::Dash,
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Options::default()
        })
        .skip_tags(vec!["script", "style"])
        .build();
    let markdown = converter.convert(&article.content).map_err(|_| not_readable())?;
    let markdown = PERMALINK.replace_all(&markdown, "");
    let markdown = HEADING_TRAIL.replace_all(&markdown, "$1").trim().to_owned();
    if markdown.is_empty() {
        return Err(not_readable());
    }
    let title = Some(article.title.trim().to_owned()).filter(|t| !t.is_empty()).unwrap_or_else(|| {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| url.to_owned())
    });
    Ok((title, markdown))
}

pub fn open(link: &str) -> Result<Doc, ArticleFailed> {
    let url = raw_markdown_url(&paper_url(link));
    let response = ureq::get(&url)
        .header("user-agent", "Mozilla/5.0 (leaf reader)")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(status) => ArticleFailed::Http { url: url.clone(), status },
            other => ArticleFailed::Fetch {
                url: url.clone(),
                cause: other.to_string(),
            },
        })?;
    let final_url = response.get_uri().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let download = |e: ureq::Error| ArticleFailed::Download {
        url: url.clone(),
        cause: e.to_string(),
    };
    let mut body = response.into_body();
    // Paper links often serve a PDF from a URL without ".pdf"; trust the content type.
    if is_pdf(content_type.as_deref(), &final_url) {
        let bytes = body.with_config().limit(MAX_DOWNLOAD_BYTES).read_to_vec().map_err(download)?;
        return Ok(pdf::from_bytes(&bytes, &url, &link_title(&final_url))?);
    }
    if is_markdown(content_type.as_deref(), &final_url) {
        let bytes = body.with_config().limit(MAX_DOWNLOAD_BYTES).read_to_vec().map_err(download)?;
        let base = Url::parse(&final_url).map_err(|e| ArticleFailed::Fetch {
            url: final_url.clone(),
            cause: e.to_string(),
        })?;
        return Ok(markdown::from_url(link_title(&final_url), &String::from_utf8_lossy(&bytes), &base));
    }
    let html = body.with_config().limit(MAX_DOWNLOAD_BYTES).read_to_string().map_err(download)?;
    let (title, markdown) = from_html(&html, &final_url)?;
    Ok(Doc::whole(title, markdown))
}

/// Downloads a remote image for a figure. `None` when it can't be fetched or decoded;
/// the text around it still shows.
pub fn fetch_image(url: &str) -> Option<image::RgbaImage> {
    let response = ureq::get(url).header("user-agent", "Mozilla/5.0 (leaf reader)").call().ok()?;
    let bytes = response.into_body().with_config().limit(64 * 1024 * 1024).read_to_vec().ok()?;
    image::load_from_memory(&bytes).ok().map(|i| i.to_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str) -> String {
        let filler = "<p>Filler sentence to look like an article, with commas, and enough text to count as content.</p>".repeat(8);
        format!("<html><head><title>Post</title></head><body><article><h1>Post</h1>{body}{filler}</article></body></html>")
    }

    #[test]
    fn relative_links_and_images_become_absolute_and_permalinks_go() {
        let html =
            page(r##"<h2>Setup <a href="#setup">#</a></h2><p>See <a href="/docs">the docs</a>.</p><img src="img/a.png" alt="diagram">"##);
        let (_, md) = from_html(&html, "https://example.com/blog/post").expect("readable");
        assert!(md.contains("## Setup\n"), "{md}");
        assert!(md.contains("[the docs](https://example.com/docs)"), "{md}");
        assert!(md.contains("![diagram](https://example.com/blog/img/a.png)"), "{md}");
    }

    #[test]
    fn a_page_with_no_readable_content_is_an_error() {
        assert!(matches!(
            from_html("<html><body></body></html>", "https://example.com/"),
            Err(ArticleFailed::NotReadable { .. })
        ));
    }

    #[test]
    fn an_arxiv_abstract_link_reads_the_paper() {
        assert_eq!(
            paper_url("https://arxiv.org/abs/1706.03762v7"),
            "https://arxiv.org/pdf/1706.03762v7"
        );
        assert_eq!(paper_url("https://example.com/abs/1"), "https://example.com/abs/1");
    }

    #[test]
    fn link_titles_come_from_the_file_name_or_host() {
        assert_eq!(link_title("https://x.org/papers/My%20Paper.pdf"), "My Paper");
        assert_eq!(link_title("https://x.org/docs/GUIDE.MD"), "GUIDE");
        assert_eq!(link_title("https://x.org/"), "x.org");
    }

    #[test]
    fn a_github_markdown_page_reads_the_raw_file() {
        assert_eq!(
            raw_markdown_url("https://github.com/o/r/blob/main/docs/README.md"),
            "https://raw.githubusercontent.com/o/r/main/docs/README.md"
        );
        assert_eq!(
            raw_markdown_url("https://github.com/o/r/blob/main/src/lib.rs"),
            "https://github.com/o/r/blob/main/src/lib.rs"
        );
        assert_eq!(raw_markdown_url("https://github.com/o/r"), "https://github.com/o/r");
    }

    #[test]
    fn markdown_is_known_by_type_or_by_extension_on_plain_text() {
        assert!(is_markdown(Some("text/markdown; charset=utf-8"), "https://x.org/notes"));
        assert!(is_markdown(
            Some("text/plain; charset=utf-8"),
            "https://raw.githubusercontent.com/o/r/main/README.md"
        ));
        assert!(!is_markdown(Some("text/html"), "https://x.org/post.md"));
        assert!(!is_markdown(Some("text/plain"), "https://x.org/notes.txt"));
    }
}
