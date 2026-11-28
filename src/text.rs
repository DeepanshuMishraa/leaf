//! Any other text file: source code, config, logs, scripts. It is shown as
//! code, colored by the language its name points to (`syntax`), in pages of
//! `LINES` lines so a long log opens at once and each page is colored on its
//! own. A file that isn't text is refused, so leaf never shows binary noise.

use std::path::Path;
use std::rc::Rc;

use image::RgbaImage;

use crate::doc::{Doc, Source};

/// Lines on a page.
const LINES: usize = 1000;
/// Bytes read to decide whether a file is text.
const SNIFF: usize = 8192;

#[derive(Debug, thiserror::Error)]
pub enum TextFailed {
    #[error(
        "Could not read {path} because the operating system denied access ({cause}). Allow your terminal to read it in your system's privacy settings, then restart it."
    )]
    Blocked { path: String, cause: std::io::Error },
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
    #[error(
        "Can't open {path}: it's a binary file, not text. leaf reads text files, PDF files (.pdf), EPUB books (.epub), markdown (.md), HTML pages (.html), Word documents (.docx), slide decks (.pptx), spreadsheets (.xlsx, .csv), pictures and web pages (http:// or https:// URLs). The file was not changed."
    )]
    Binary { path: String },
}

/// Whether `bytes`, the start of a file, look like text: no NUL bytes, and
/// UTF-8 apart from a character cut off at the end.
fn is_text(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SNIFF)];
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() == SNIFF,
    }
}

/// The fence language for a file, from its extension or its whole name.
/// `None` leaves the language to be guessed from the code.
fn language(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.to_ascii_lowercase();
    let by_name = match name.as_str() {
        "makefile" | "gnumakefile" => Some("makefile"),
        "dockerfile" | "containerfile" => Some("dockerfile"),
        "cmakelists.txt" => Some("cmake"),
        "gemfile" | "rakefile" | "podfile" | "vagrantfile" | "brewfile" => Some("ruby"),
        ".bashrc" | ".bash_profile" | ".profile" | ".zshrc" | ".zprofile" | ".zshenv" => {
            Some("bash")
        }
        "cargo.lock" | "pipfile" => Some("toml"),
        _ => None,
    };
    if let Some(lang) = by_name {
        return Some(lang.to_owned());
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    crate::syntax::knows(&ext).then_some(ext)
}

/// A fence of backticks longer than any run of them in `text`, so the text
/// can't close it early.
fn fence(text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

struct Text {
    /// Each page's markdown: one fenced code block.
    pages: Vec<Rc<str>>,
}

impl Source for Text {
    fn page(&self, index: usize) -> Rc<str> {
        self.pages.get(index).cloned().unwrap_or_else(|| "".into())
    }
    fn image_size(&self, _id: &str) -> Option<(u32, u32)> {
        None
    }
    fn decode(&self, _id: &str, _width: u32) -> Option<RgbaImage> {
        None
    }
}

/// The pages of `text`, each a code block in `lang`.
fn pages(text: &str, lang: Option<&str>) -> Vec<Rc<str>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = text.split('\n').collect();
    lines
        .chunks(LINES)
        .map(|chunk| {
            let code = chunk.join("\n");
            let fence = fence(&code);
            format!("{fence}{}\n{code}\n{fence}\n", lang.unwrap_or("")).into()
        })
        .collect()
}

pub fn open(path: &Path) -> Result<Doc, TextFailed> {
    let shown = || path.display().to_string();
    let bytes = std::fs::read(path).map_err(|cause| {
        if cause.kind() == std::io::ErrorKind::PermissionDenied {
            TextFailed::Blocked {
                path: shown(),
                cause,
            }
        } else {
            TextFailed::Read {
                path: shown(),
                cause,
            }
        }
    })?;
    if !is_text(&bytes) {
        return Err(TextFailed::Binary { path: shown() });
    }
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let pages = pages(&text, language(path).as_deref());
    let title = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Doc {
        title,
        page_count: (pages.len() > 1).then_some(pages.len()),
        bookmarks: Vec::new(),
        source: Box::new(Text { pages }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nul_bytes_and_broken_utf8_mean_binary() {
        assert!(is_text(b"fn main() {}\n"));
        assert!(is_text("héllo".as_bytes()));
        assert!(!is_text(b"\x7fELF\x02\x01\x00\x00"));
        assert!(!is_text(b"\xff\xfe\xfd text"));
    }

    #[test]
    fn a_character_cut_at_the_sniff_limit_is_still_text() {
        let mut bytes = vec![b'a'; SNIFF - 1];
        bytes.extend_from_slice("é".as_bytes());
        assert!(is_text(&bytes));
    }

    #[test]
    fn the_language_comes_from_the_extension_or_the_whole_name() {
        assert_eq!(language(Path::new("a/b.py")).as_deref(), Some("py"));
        assert_eq!(language(Path::new("run.BAT")).as_deref(), Some("bat"));
        assert_eq!(
            language(Path::new("x/Makefile")).as_deref(),
            Some("makefile")
        );
        assert_eq!(
            language(Path::new("Dockerfile")).as_deref(),
            Some("dockerfile")
        );
        assert_eq!(language(Path::new("notes.unknownext")), None);
        assert_eq!(language(Path::new("LICENSE")), None);
    }

    #[test]
    fn backticks_in_the_text_get_a_longer_fence() {
        assert_eq!(fence("plain"), "```");
        assert_eq!(fence("a ```b``` c"), "````");
    }

    #[test]
    fn long_files_split_into_pages_of_whole_lines() {
        let text: String = (0..LINES + 5).map(|i| format!("line {i}\n")).collect();
        let pages = pages(&text, Some("rust"));
        assert_eq!(pages.len(), 2);
        assert!(pages[0].starts_with("```rust\nline 0\n"));
        assert!(pages[0].ends_with(&format!("line {}\n```\n", LINES - 1)));
        assert!(pages[1].starts_with(&format!("```rust\nline {LINES}\n")));
    }

    #[test]
    fn opening_a_script_shows_it_as_code_and_binary_files_are_refused() {
        let dir = std::env::temp_dir().join(format!("leaf-text-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("go.bat");
        std::fs::write(&script, "@echo off\r\necho hi\r\n").unwrap();
        let doc = open(&script).unwrap();
        assert_eq!(doc.title, "go.bat");
        assert_eq!(&*doc.source.page(0), "```bat\n@echo off\necho hi\n```\n");

        let blob = dir.join("blob.bin");
        std::fs::write(&blob, [0u8, 1, 2, 3]).unwrap();
        assert!(matches!(open(&blob), Err(TextFailed::Binary { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
