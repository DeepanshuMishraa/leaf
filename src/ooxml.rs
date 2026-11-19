//! Office Open XML packages (`.docx`, `.pptx`, `.xlsx`): the zip of XML parts,
//! the relationships between them, and the theme colors and fonts they share.
//! Read by leaf itself, so these files open the same everywhere, at once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::path::Path;

pub use resvg::usvg::roxmltree;
use roxmltree::Node;

/// EMUs (English Metric Units) in a point.
pub const EMU_PER_PT: f32 = 12_700.0;

pub fn emu(value: &str) -> Option<f32> {
    value
        .parse::<f64>()
        .ok()
        .map(|v| (v / EMU_PER_PT as f64) as f32)
}

/// Largest part read, uncompressed: past any real slide, sheet or picture.
const MAX_PART_BYTES: u64 = 512 << 20;

/// An open package. Parts are read on demand.
pub struct Package {
    zip: RefCell<zip::ZipArchive<Cursor<Vec<u8>>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PackageFailed {
    #[error("Could not read {path} ({cause}). Check that the path is right and the file exists.")]
    Read { path: String, cause: std::io::Error },
    #[error(
        "{path} isn't a readable {kind} file: {detail}. It may be damaged, password-protected, or an older format renamed. Open it in an office app and save it again. The file was not changed."
    )]
    Invalid {
        path: String,
        kind: &'static str,
        detail: String,
    },
}

/// A link from one part to another, with its target resolved to a part name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rel {
    pub id: String,
    /// The relationship type's last segment, such as `slideLayout` or `image`.
    pub kind: String,
    pub target: String,
    pub external: bool,
}

impl Package {
    pub fn open(path: &Path, kind: &'static str) -> Result<Package, PackageFailed> {
        let shown = || path.display().to_string();
        let bytes = std::fs::read(path).map_err(|cause| PackageFailed::Read {
            path: shown(),
            cause,
        })?;
        let zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| PackageFailed::Invalid {
            path: shown(),
            kind,
            detail: format!("it isn't a zip package ({e})"),
        })?;
        Ok(Package {
            zip: RefCell::new(zip),
        })
    }

    pub fn bytes(&self, part: &str) -> Option<Vec<u8>> {
        let mut zip = self.zip.borrow_mut();
        let file = zip.by_name(part.trim_start_matches('/')).ok()?;
        // The size a part claims isn't trusted: a damaged or hostile file
        // could claim terabytes. Read up to the limit, and give up past it.
        let mut out = Vec::with_capacity((file.size() as usize).min(1 << 20));
        file.take(MAX_PART_BYTES + 1).read_to_end(&mut out).ok()?;
        (out.len() as u64 <= MAX_PART_BYTES).then_some(out)
    }

    pub fn text(&self, part: &str) -> Option<String> {
        String::from_utf8(self.bytes(part)?).ok()
    }

    /// The relationships of `part`, from its `_rels` file.
    pub fn rels(&self, part: &str) -> Vec<Rel> {
        let (dir, name) = part.rsplit_once('/').unwrap_or(("", part));
        let rels_part = if dir.is_empty() {
            format!("_rels/{name}.rels")
        } else {
            format!("{dir}/_rels/{name}.rels")
        };
        let Some(xml) = self.text(&rels_part) else {
            return Vec::new();
        };
        let Ok(doc) = roxmltree::Document::parse(&xml) else {
            return Vec::new();
        };
        doc.root_element()
            .children()
            .filter(|n| n.has_tag_name_local("Relationship"))
            .filter_map(|n| {
                let target = n.attribute("Target")?;
                let external = n.attribute("TargetMode") == Some("External");
                Some(Rel {
                    id: n.attribute("Id")?.to_owned(),
                    kind: n
                        .attribute("Type")?
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_owned(),
                    target: if external {
                        target.to_owned()
                    } else {
                        join(dir, target)
                    },
                    external,
                })
            })
            .collect()
    }
}

/// `target` resolved against the folder `dir`, as a part name.
pub fn join(dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_owned();
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            part => parts.push(part),
        }
    }
    parts.join("/")
}

pub fn rel_by_id<'a>(rels: &'a [Rel], id: &str) -> Option<&'a Rel> {
    rels.iter().find(|r| r.id == id)
}

pub fn rel_by_kind<'a>(rels: &'a [Rel], kind: &str) -> Option<&'a Rel> {
    rels.iter().find(|r| r.kind == kind)
}

/// Lookups by local name, ignoring XML namespaces.
pub trait Local<'a, 'i> {
    fn has_tag_name_local(&self, name: &str) -> bool;
    fn child(&self, name: &str) -> Option<Node<'a, 'i>>;
    fn kids(&self, name: &'static str) -> Box<dyn Iterator<Item = Node<'a, 'i>> + 'a>;
    fn path(&self, names: &[&str]) -> Option<Node<'a, 'i>>;
    /// An attribute by local name, whatever its namespace.
    fn attr(&self, name: &str) -> Option<&'a str>;
    fn local(&self) -> &'a str;
}

impl<'a, 'i: 'a> Local<'a, 'i> for Node<'a, 'i> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }

    fn child(&self, name: &str) -> Option<Node<'a, 'i>> {
        self.children().find(|n| n.has_tag_name_local(name))
    }

    fn kids(&self, name: &'static str) -> Box<dyn Iterator<Item = Node<'a, 'i>> + 'a> {
        Box::new(self.children().filter(move |n| n.has_tag_name_local(name)))
    }

    fn path(&self, names: &[&str]) -> Option<Node<'a, 'i>> {
        let mut node = *self;
        for name in names {
            node = node.child(name)?;
        }
        Some(node)
    }

    fn attr(&self, name: &str) -> Option<&'a str> {
        self.attributes()
            .find(|a| a.name() == name)
            .map(|a| a.value())
    }

    fn local(&self) -> &'a str {
        self.tag_name().name()
    }
}

/// An sRGB color with opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    pub rgb: [u8; 3],
    pub alpha: f32,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color {
            rgb: [r, g, b],
            alpha: 1.0,
        }
    }

    pub fn hex(text: &str) -> Option<Color> {
        let text = text.trim_start_matches('#');
        if text.len() != 6 {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(text.get(i..i + 2)?, 16).ok();
        Some(Color::rgb(byte(0)?, byte(2)?, byte(4)?))
    }

    /// `#rrggbb` for SVG.
    pub fn css(&self) -> String {
        let [r, g, b] = self.rgb;
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    /// Relative luminance, 0 (black) to 1 (white).
    pub fn luminance(&self) -> f32 {
        let [r, g, b] = self.rgb.map(|c| c as f32 / 255.0);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn hsl(&self) -> (f32, f32, f32) {
        let [r, g, b] = self.rgb.map(|c| c as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        if max == min {
            return (0.0, 0.0, l);
        }
        let d = max - min;
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let h = if max == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h / 6.0, s, l)
    }

    fn from_hsl(h: f32, s: f32, l: f32, alpha: f32) -> Color {
        let channel = |t: f32| {
            let t = t.rem_euclid(1.0);
            let q = if l < 0.5 {
                l * (1.0 + s)
            } else {
                l + s - l * s
            };
            let p = 2.0 * l - q;
            let v = if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            };
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        };
        Color {
            rgb: [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)],
            alpha,
        }
    }

    /// The color lightened (positive) or darkened (negative) by a
    /// spreadsheet's `tint`, from -1 to 1.
    pub fn tinted(self, tint: f32) -> Color {
        let (h, s, l) = self.hsl();
        let l = if tint < 0.0 {
            l * (1.0 + tint)
        } else {
            l * (1.0 - tint) + tint
        };
        Color::from_hsl(h, s, l.clamp(0.0, 1.0), self.alpha)
    }

    /// The color with DrawingML's modifiers applied, in document order.
    fn modified(self, node: Node) -> Color {
        let mut color = self;
        for m in node.children().filter(|n| n.is_element()) {
            let val = m
                .attribute("val")
                .and_then(|v| v.parse::<f32>().ok())
                .map(|v| v / 100_000.0);
            let Some(val) = val else { continue };
            match m.local() {
                "alpha" => color.alpha = val.clamp(0.0, 1.0),
                "lumMod" => {
                    let (h, s, l) = color.hsl();
                    color = Color::from_hsl(h, s, (l * val).clamp(0.0, 1.0), color.alpha);
                }
                "lumOff" => {
                    let (h, s, l) = color.hsl();
                    color = Color::from_hsl(h, s, (l + val).clamp(0.0, 1.0), color.alpha);
                }
                "satMod" => {
                    let (h, s, l) = color.hsl();
                    color = Color::from_hsl(h, (s * val).clamp(0.0, 1.0), l, color.alpha);
                }
                "tint" => {
                    color.rgb = color
                        .rgb
                        .map(|c| (c as f32 + (255.0 - c as f32) * (1.0 - val)).round() as u8);
                }
                "shade" => {
                    color.rgb = color.rgb.map(|c| (c as f32 * val).round() as u8);
                }
                _ => {}
            }
        }
        color
    }
}

/// A theme's colors and fonts, with the master's mapping of `bg1`, `tx1`
/// and so on to the theme's `lt1`, `dk1`.
#[derive(Debug, Clone, Default)]
pub struct Theme {
    colors: HashMap<String, Color>,
    map: HashMap<String, String>,
    /// Headings' typeface.
    pub major: String,
    /// Body text's typeface.
    pub minor: String,
    /// Fill styles, for shapes that name one by index.
    pub fills: Vec<String>,
    /// Background fill styles (indexes from 1001).
    pub backgrounds: Vec<String>,
}

impl Theme {
    pub fn parse(xml: &str) -> Theme {
        let mut theme = Theme {
            major: "Calibri Light".into(),
            minor: "Calibri".into(),
            ..Theme::default()
        };
        let Ok(doc) = roxmltree::Document::parse(xml) else {
            return theme;
        };
        let root = doc.root_element();
        let Some(elements) = root.child("themeElements") else {
            return theme;
        };
        if let Some(scheme) = elements.child("clrScheme") {
            for slot in scheme.children().filter(|n| n.is_element()) {
                let color =
                    slot.children()
                        .find(|n| n.is_element())
                        .and_then(|c| match c.local() {
                            "srgbClr" => c.attribute("val").and_then(Color::hex),
                            "sysClr" => c.attribute("lastClr").and_then(Color::hex),
                            _ => None,
                        });
                if let Some(color) = color {
                    theme.colors.insert(slot.local().to_owned(), color);
                }
            }
        }
        if let Some(fonts) = elements.child("fontScheme") {
            let face = |which: &str| {
                fonts
                    .path(&[which, "latin"])
                    .and_then(|n| n.attribute("typeface"))
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
            };
            if let Some(major) = face("majorFont") {
                theme.major = major;
            }
            if let Some(minor) = face("minorFont") {
                theme.minor = minor;
            }
        }
        if let Some(format) = elements.child("fmtScheme") {
            let list = |name: &str| -> Vec<String> {
                format
                    .child(name)
                    .map(|l| {
                        l.children()
                            .filter(|n| n.is_element())
                            .map(|n| xml[n.range()].to_owned())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            theme.fills = list("fillStyleLst");
            theme.backgrounds = list("bgFillStyleLst");
        }
        theme
    }

    /// Sets how `bg1`, `tx1`, `bg2`, `tx2` map to theme slots, from a
    /// `clrMap` element.
    pub fn map_colors(&mut self, clr_map: Node) {
        for a in clr_map.attributes() {
            self.map.insert(a.name().to_owned(), a.value().to_owned());
        }
    }

    pub fn scheme(&self, name: &str) -> Option<Color> {
        let slot = match name {
            "bg1" | "tx1" | "bg2" | "tx2" => {
                self.map
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or(match name {
                        "bg1" => "lt1",
                        "tx1" => "dk1",
                        "bg2" => "lt2",
                        _ => "dk2",
                    })
            }
            other => other,
        };
        self.colors.get(slot).copied()
    }

    /// A typeface name with theme references (`+mj-lt`, `+mn-lt`) resolved.
    pub fn typeface<'a>(&'a self, name: &'a str) -> &'a str {
        match name {
            "+mj-lt" | "+mj-ea" | "+mj-cs" => &self.major,
            "+mn-lt" | "+mn-ea" | "+mn-cs" => &self.minor,
            other => other,
        }
    }

    /// The color an element such as `solidFill` holds, with `placeholder`
    /// standing in for `phClr`.
    pub fn color(&self, holder: Node, placeholder: Option<Color>) -> Option<Color> {
        let c = holder.children().find(|n| n.is_element())?;
        let base =
            match c.local() {
                "srgbClr" => c.attribute("val").and_then(Color::hex),
                "sysClr" => c.attribute("lastClr").and_then(Color::hex).or_else(|| {
                    match c.attribute("val") {
                        Some("window") => Some(Color::WHITE),
                        _ => Some(Color::BLACK),
                    }
                }),
                "schemeClr" => match c.attribute("val") {
                    Some("phClr") => placeholder,
                    Some(name) => self.scheme(name),
                    None => None,
                },
                "prstClr" => preset(c.attribute("val").unwrap_or("")),
                "scrgbClr" => {
                    let pct = |n: &str| {
                        c.attribute(n)
                            .and_then(|v| v.parse::<f32>().ok())
                            .map(|v| (v / 100_000.0).clamp(0.0, 1.0))
                    };
                    Some(Color::rgb(
                        (pct("r")? * 255.0) as u8,
                        (pct("g")? * 255.0) as u8,
                        (pct("b")? * 255.0) as u8,
                    ))
                }
                _ => None,
            }?;
        Some(base.modified(c))
    }
}

fn preset(name: &str) -> Option<Color> {
    Some(match name {
        "black" => Color::BLACK,
        "white" => Color::WHITE,
        "red" => Color::rgb(255, 0, 0),
        "green" => Color::rgb(0, 128, 0),
        "blue" => Color::rgb(0, 0, 255),
        "yellow" => Color::rgb(255, 255, 0),
        "gray" | "grey" => Color::rgb(128, 128, 128),
        "lightGray" | "ltGray" => Color::rgb(211, 211, 211),
        "darkGray" | "dkGray" => Color::rgb(169, 169, 169),
        "orange" => Color::rgb(255, 165, 0),
        "purple" => Color::rgb(128, 0, 128),
        _ => return None,
    })
}

/// Text escaped for XML content and attribute values.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => out.push(c),
        }
    }
    out
}

/// Text taken literally in markdown.
pub fn md_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\`*_[]<>#|~&$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// An image's media type for a data URI, by its bytes; `None` for formats
/// the renderer can't draw (EMF, WMF).
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else if bytes
        .iter()
        .take(512)
        .copied()
        .collect::<Vec<u8>>()
        .windows(4)
        .any(|w| w == b"<svg")
    {
        Some("image/svg+xml")
    } else {
        None
    }
}

/// A data URI for an image's bytes. BMP and TIFF are converted to PNG,
/// since SVG renderers only take PNG, JPEG, GIF, WebP and SVG.
pub fn data_uri(bytes: &[u8]) -> Option<String> {
    use base64::Engine;
    let encode = |mime: &str, data: &[u8]| {
        format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(data)
        )
    };
    if let Some(mime) = image_mime(bytes) {
        return Some(encode(mime, bytes));
    }
    let image = image::load_from_memory(bytes).ok()?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(encode("image/png", &png))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_against_their_folder() {
        assert_eq!(join("ppt", "slides/slide1.xml"), "ppt/slides/slide1.xml");
        assert_eq!(
            join("ppt/slides", "../media/image1.png"),
            "ppt/media/image1.png"
        );
        assert_eq!(join("ppt/slides", "/ppt/media/a.png"), "ppt/media/a.png");
        assert_eq!(join("", "word/document.xml"), "word/document.xml");
    }

    #[test]
    fn theme_colors_follow_the_map_and_modifiers() {
        let mut theme = Theme::parse(
            r#"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:themeElements>
            <a:clrScheme name="x"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
            <a:lt1><a:srgbClr val="FFFFFF"/></a:lt1><a:accent1><a:srgbClr val="4472C4"/></a:accent1></a:clrScheme>
            <a:fontScheme name="f"><a:majorFont><a:latin typeface="Aptos Display"/></a:majorFont><a:minorFont><a:latin typeface="Aptos"/></a:minorFont></a:fontScheme>
            </a:themeElements></a:theme>"#,
        );
        assert_eq!(theme.scheme("tx1"), Some(Color::BLACK));
        assert_eq!(theme.scheme("bg1"), Some(Color::WHITE));
        assert_eq!(theme.typeface("+mj-lt"), "Aptos Display");
        assert_eq!(theme.typeface("+mn-lt"), "Aptos");
        let doc =
            roxmltree::Document::parse(r#"<p:clrMap xmlns:p="p" bg1="dk1" tx1="lt1"/>"#).unwrap();
        theme.map_colors(doc.root_element());
        assert_eq!(theme.scheme("bg1"), Some(Color::BLACK));
        let fill = roxmltree::Document::parse(
            r#"<a:solidFill xmlns:a="a"><a:schemeClr val="accent1"><a:lumMod val="50000"/></a:schemeClr></a:solidFill>"#,
        )
        .unwrap();
        let darker = theme.color(fill.root_element(), None).unwrap();
        assert!(darker.luminance() < Color::hex("4472C4").unwrap().luminance());
    }

    #[test]
    fn hsl_round_trips() {
        let c = Color::rgb(68, 114, 196);
        let (h, s, l) = c.hsl();
        assert_eq!(Color::from_hsl(h, s, l, 1.0), c);
    }
}
