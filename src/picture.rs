//! A picture file opened on its own: one page holding the image, so `i`
//! switches it between its own colors and the theme's, as in any document.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use image::RgbaImage;

use crate::doc::{Doc, Source};

/// Extensions leaf may open as pictures. HEIC, HEIF, AVIF and JPEG XL need
/// macOS's `sips`, so `is_picture` rejects them elsewhere rather than
/// failing to decode them; the shell completions gate them the same way.
pub const EXTENSIONS: [&str; 27] = [
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "pnm",
    "pbm", "pgm", "ppm", "pam", "qoi", "hdr", "exr", "dds", "ff", "svg", "jxl", "heic", "heif",
    "avif",
];

/// The extensions only macOS's `sips` can read.
const SIPS_ONLY: [&str; 4] = ["heic", "heif", "avif", "jxl"];

#[derive(Debug, thiserror::Error)]
pub enum PictureFailed {
    #[error(
        "Could not read {path} because the operating system denied access ({cause}). Allow your terminal to read it in your system's privacy settings, then restart it."
    )]
    Blocked { path: String, cause: std::io::Error },
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
    #[error(
        "Could not decode {path}: it isn't a picture leaf can read, or the file is damaged. The file was not changed."
    )]
    Undecodable { path: String },
}

pub fn is_picture(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x))
            && (cfg!(target_os = "macos") || !SIPS_ONLY.iter().any(|x| e.eq_ignore_ascii_case(x)))
    })
}

/// Holds the path and size, not the pixels: a large photo would otherwise
/// keep its full-resolution decode in memory while the document is open.
struct Picture {
    text: Rc<str>,
    path: PathBuf,
    size: (u32, u32),
}

impl Source for Picture {
    fn page(&self, _index: usize) -> Rc<str> {
        self.text.clone()
    }

    fn image_size(&self, _id: &str) -> Option<(u32, u32)> {
        Some(self.size)
    }

    fn decode(&self, _id: &str, width: u32) -> Option<RgbaImage> {
        let pixels = pixels(&self.path, &std::fs::read(&self.path).ok()?)?;
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
}

pub fn open(path: &Path) -> Result<Doc, PictureFailed> {
    let shown = || path.display().to_string();
    let bytes = std::fs::read(path).map_err(|cause| {
        if cause.kind() == std::io::ErrorKind::PermissionDenied {
            PictureFailed::Blocked {
                path: shown(),
                cause,
            }
        } else {
            PictureFailed::Read {
                path: shown(),
                cause,
            }
        }
    })?;
    let size = pixels(path, &bytes)
        .ok_or_else(|| PictureFailed::Undecodable { path: shown() })?
        .dimensions();
    Ok(Doc {
        title: path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        page_count: None,
        source: Box::new(Picture {
            text: format!("![]({})", Doc::image_href("0")).into(),
            path: path.to_path_buf(),
            size,
        }),
    })
}

fn pixels(path: &Path, bytes: &[u8]) -> Option<RgbaImage> {
    crate::prmedia::decode(bytes).or_else(|| through_sips(path))
}

/// Formats the image crate can't read (HEIC, AVIF, JPEG XL), converted to PNG
/// by macOS's `sips`.
fn through_sips(path: &Path) -> Option<RgbaImage> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = std::env::temp_dir().join(format!("leaf-picture-{}.png", std::process::id()));
    let converted = Command::new("sips")
        .args(["-s", "format", "png"])
        .arg(path)
        .arg("--out")
        .arg(&out)
        .output()
        .is_ok_and(|o| o.status.success());
    let pixels = converted
        .then(|| image::open(&out).ok())
        .flatten()
        .map(|image| image.to_rgba8());
    let _ = std::fs::remove_file(&out);
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_a_picture_as_one_image_page() {
        let dir = std::env::temp_dir().join(format!("leaf-picture-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dot.bmp");
        RgbaImage::from_pixel(40, 20, image::Rgba([200, 10, 10, 255]))
            .save(&path)
            .unwrap();
        let doc = open(&path).unwrap();
        assert_eq!(doc.title, "dot.bmp");
        assert_eq!(&*doc.source.page(0), "![](leaf-image:0)");
        assert_eq!(doc.source.image_size("0"), Some((40, 20)));
        assert_eq!(
            doc.source.decode("0", 10).map(|p| p.dimensions()),
            Some((10, 5))
        );
        assert_eq!(
            doc.source.decode("0", 100).map(|p| p.dimensions()),
            Some((40, 20))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn garbage_is_reported_not_shown() {
        let dir = std::env::temp_dir().join(format!("leaf-picture-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.png");
        std::fs::write(&path, b"not a picture").unwrap();
        assert!(matches!(
            open(&path),
            Err(PictureFailed::Undecodable { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recognizes_extensions_in_any_case() {
        assert!(is_picture(Path::new("a/B.JPG")));
        assert_eq!(is_picture(Path::new("x.heic")), cfg!(target_os = "macos"));
        assert!(!is_picture(Path::new("notes.md")));
    }
}
