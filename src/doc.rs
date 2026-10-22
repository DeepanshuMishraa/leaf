//! A readable document: markdown pages plus the images they reference.

use std::rc::Rc;

use image::RgbaImage;

use crate::mono::Rgb;

const IMAGE_SCHEME: &str = "leaf-image:";

/// Where a markdown image href points.
pub enum ImageRef<'a> {
    /// An image embedded in the document, by id.
    Embedded(&'a str),
    /// An image on the web.
    Remote(&'a str),
}

/// What a document is made of. Pages are read on demand, so a book of
/// thousands of pages opens at once and only the pages being read cost
/// anything. Embedded images are decoded only when shown.
pub trait Source {
    /// Markdown of the page at `index` (0-based).
    fn page(&self, index: usize) -> Rc<str>;
    /// Pixel size of an embedded image, known without decoding it.
    fn image_size(&self, id: &str) -> Option<(u32, u32)>;
    /// The pixels of an embedded image, at most `width` pixels wide, or `None`
    /// if it can't be decoded.
    fn decode(&self, id: &str, width: u32) -> Option<RgbaImage>;
    /// Whether an embedded image is a diagram, drawn in the terminal's colors.
    fn is_diagram(&self, _id: &str) -> bool {
        false
    }
    /// A diagram drawn in `ink` on a transparent `paper`, at most `width` pixels wide.
    fn draw(&self, _id: &str, _width: u32, _ink: Rgb, _paper: Rgb) -> Option<RgbaImage> {
        None
    }
}

pub struct Doc {
    pub title: String,
    /// Total pages, when the source has pages; a source without pages is one page.
    pub page_count: Option<usize>,
    pub source: Box<dyn Source>,
}

impl Doc {
    /// A document whose markdown is known up front.
    pub fn whole(title: impl Into<String>, markdown: impl Into<String>) -> Self {
        Doc {
            title: title.into(),
            page_count: None,
            source: Box::new(Whole(markdown.into().into())),
        }
    }

    /// How many pages the viewer lays out.
    pub fn pages(&self) -> usize {
        self.page_count.unwrap_or(1)
    }

    /// The markdown href for an embedded image.
    pub fn image_href(id: &str) -> String {
        format!("{IMAGE_SCHEME}{id}")
    }

    pub fn image_ref(href: &str) -> ImageRef<'_> {
        match href.strip_prefix(IMAGE_SCHEME) {
            Some(id) => ImageRef::Embedded(id),
            None => ImageRef::Remote(href),
        }
    }
}

struct Whole(Rc<str>);

impl Source for Whole {
    fn page(&self, _index: usize) -> Rc<str> {
        self.0.clone()
    }
    fn image_size(&self, _id: &str) -> Option<(u32, u32)> {
        None
    }
    fn decode(&self, _id: &str, _width: u32) -> Option<RgbaImage> {
        None
    }
}
