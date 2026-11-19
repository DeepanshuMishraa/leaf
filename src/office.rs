//! Office files: Word documents, slide decks and spreadsheets, opened for
//! reading. Edit them in Word, PowerPoint, Excel, Google's apps or
//! LibreOffice; leaf shows them.
//!
//! leaf reads the files itself, on every platform, with nothing else
//! installed. Decks and documents can be shown two ways (`o` switches): as
//! designed, in their own fonts, colors and layout, or as text in the
//! terminal's font. Spreadsheets too: as a sheet, or as tables.

use std::path::Path;

use crate::doc::Doc;

/// What an office file holds, which decides how it's shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A word-processing document.
    Words,
    /// A slide deck, shown slide by slide.
    Slides,
    /// A spreadsheet.
    Sheets,
}

const WORDS: [&str; 4] = ["docx", "docm", "dotx", "dotm"];
const SLIDES: [&str; 6] = ["pptx", "pptm", "ppsx", "ppsm", "potx", "potm"];
const SHEETS: [&str; 9] = [
    "xlsx", "xlsm", "xlsb", "xls", "xla", "xlam", "ods", "csv", "tsv",
];

/// Formats leaf can't read, and the format to save them as instead.
const OLDER: [(&str, &str); 9] = [
    ("doc", ".docx"),
    ("rtf", ".docx"),
    ("odt", ".docx"),
    ("pages", ".docx"),
    ("ppt", ".pptx"),
    ("pps", ".pptx"),
    ("odp", ".pptx"),
    ("key", ".pptx"),
    ("numbers", ".xlsx"),
];

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

impl Kind {
    pub fn of(path: &Path) -> Option<Kind> {
        let ext = extension(path)?;
        let is = |list: &[&str]| list.contains(&ext.as_str());
        if is(&WORDS) {
            Some(Kind::Words)
        } else if is(&SLIDES) {
            Some(Kind::Slides)
        } else if is(&SHEETS) {
            Some(Kind::Sheets)
        } else {
            None
        }
    }
}

/// Advice for an office format leaf doesn't read, such as `.doc` or `.key`.
pub fn older(path: &Path) -> Option<String> {
    let ext = extension(path)?;
    let (_, instead) = OLDER.iter().find(|(e, _)| *e == ext)?;
    Some(format!(
        "leaf can't read .{ext} files like {}. Save it as {instead} from the app that made it (File → Save As or Export), then open that. The file was not changed.",
        path.display()
    ))
}

/// Opens an office file of any kind.
pub fn open(path: &Path) -> Result<Doc, String> {
    match Kind::of(path) {
        Some(Kind::Words) => crate::word::open(path).map_err(|e| e.to_string()),
        Some(Kind::Slides) => crate::slides::open(path).map_err(|e| e.to_string()),
        Some(Kind::Sheets) => crate::sheet::open(path).map_err(|e| e.to_string()),
        None => Err(older(path).unwrap_or_else(|| {
            format!(
                "{} isn't a Word document, slide deck or spreadsheet leaf reads.",
                path.display()
            )
        })),
    }
}

/// A picture's size in pixels, read without decoding all of it.
pub fn picture_size(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .or_else(|| crate::prmedia::decode(bytes).map(|p| p.dimensions()))
}

/// A picture's pixels, scaled down to at most `width` pixels wide.
pub fn picture_pixels(bytes: &[u8], width: u32) -> Option<image::RgbaImage> {
    let pixels = crate::prmedia::decode(bytes)?;
    let width = width.max(1);
    if pixels.width() <= width {
        return Some(pixels);
    }
    let height = (pixels.height() as u64 * width as u64 / pixels.width() as u64).max(1) as u32;
    Some(image::imageops::resize(
        &pixels,
        width,
        height,
        image::imageops::FilterType::Triangle,
    ))
}

pub fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_extensions_into_kinds_in_any_case() {
        assert_eq!(Kind::of(Path::new("a/Report.DOCX")), Some(Kind::Words));
        assert_eq!(Kind::of(Path::new("talk.pptx")), Some(Kind::Slides));
        assert_eq!(Kind::of(Path::new("budget.xlsx")), Some(Kind::Sheets));
        assert_eq!(Kind::of(Path::new("old.xls")), Some(Kind::Sheets));
        assert_eq!(Kind::of(Path::new("data.csv")), Some(Kind::Sheets));
        assert_eq!(Kind::of(Path::new("book.pdf")), None);
        assert_eq!(Kind::of(Path::new("talk.key")), None);
    }

    #[test]
    fn older_formats_say_what_to_save_as() {
        assert!(
            older(Path::new("a.doc"))
                .unwrap()
                .contains("Save it as .docx")
        );
        assert!(older(Path::new("a.KEY")).unwrap().contains(".pptx"));
        assert_eq!(older(Path::new("a.pdf")), None);
    }
}
