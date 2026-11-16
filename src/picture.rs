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

/// The pictures beside `path` (itself included), in the order a file manager
/// lists them: names compared without regard to case, with numbers counted as
/// numbers, so `img2` comes before `img10`. Hidden files are left out.
pub fn in_folder(path: &Path) -> Vec<PathBuf> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| {
            let hidden = p
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'));
            !hidden && is_picture(p) && p.is_file()
        })
        .collect();
    found.sort_by(|a, b| natural(&name_of(a), &name_of(b)).then_with(|| a.cmp(b)));
    found
}

/// The picture `step` places along from `path` in its folder, going round
/// past either end. `None` when `path` has no neighbour.
pub fn neighbour(path: &Path, step: i32) -> Option<PathBuf> {
    let all = in_folder(path);
    let here = all.iter().position(|p| p == path)?;
    let next = (here as i64 + step as i64).rem_euclid(all.len() as i64) as usize;
    (next != here).then(|| all[next].clone())
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Compares names with each run of digits taken as a number.
fn natural(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(d) = chars.next_if(|c| c.is_ascii_digit()) {
                        digits.push(d);
                    }
                    digits.trim_start_matches('0').to_owned()
                };
                let (x, y) = (number(&mut a), number(&mut b));
                // A longer number, with no leading zeros, is the bigger one.
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(&y));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
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

    fn folder_of(names: &[&str], tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-folder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in names {
            RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
                .save_with_format(dir.join(name), image::ImageFormat::Bmp)
                .unwrap();
        }
        dir
    }

    #[test]
    fn a_folder_lists_pictures_with_numbers_counted_as_numbers() {
        let dir = folder_of(
            &["img10.bmp", "IMG2.bmp", "img1.bmp", ".hidden.bmp"],
            "order",
        );
        std::fs::write(dir.join("notes.txt"), "not a picture").unwrap();
        let names: Vec<String> = in_folder(&dir.join("img1.bmp"))
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["img1.bmp", "IMG2.bmp", "img10.bmp"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn neighbours_go_round_past_either_end() {
        let dir = folder_of(&["a.bmp", "b.bmp", "c.bmp"], "round");
        let at = |n: &str| dir.join(n);
        assert_eq!(neighbour(&at("a.bmp"), 1), Some(at("b.bmp")));
        assert_eq!(neighbour(&at("c.bmp"), 1), Some(at("a.bmp")));
        assert_eq!(neighbour(&at("a.bmp"), -1), Some(at("c.bmp")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_lone_picture_has_no_neighbour() {
        let dir = folder_of(&["only.bmp"], "lone");
        assert_eq!(neighbour(&dir.join("only.bmp"), 1), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn natural_order_ignores_leading_zeros_and_case() {
        use std::cmp::Ordering::*;
        assert_eq!(natural("img2", "img10"), Less);
        assert_eq!(natural("img007", "img7"), Equal);
        assert_eq!(natural("a", "ab"), Less);
    }

    #[test]
    fn recognizes_extensions_in_any_case() {
        assert!(is_picture(Path::new("a/B.JPG")));
        assert_eq!(is_picture(Path::new("x.heic")), cfg!(target_os = "macos"));
        assert!(!is_picture(Path::new("notes.md")));
    }
}
