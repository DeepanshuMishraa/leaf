//! Kitty graphics drawn as direct placements.
//!
//! ratatui-image draws kitty images through unicode placeholders, which some
//! terminals (Den among them) show as glyphs around and through the image.
//! Placing images at the cursor needs nothing written into the cells.

use std::io::{self, Write};

use base64::Engine;
use image::RgbaImage;
use ratatui::layout::Rect;

/// Bytes of base64 per escape sequence, as the protocol allows.
const CHUNK: usize = 4096;

/// An image the terminal holds, and the cells it's drawn over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Default)]
pub struct Kitty {
    next_id: u32,
    /// Uploads and deletions waiting for the next flush.
    pending: Vec<u8>,
    placed: Vec<(u32, Rect)>,
}

impl Kitty {
    /// Queues `pixels` for upload, to be drawn over `cols` × `rows` cells.
    pub fn upload(&mut self, pixels: &RgbaImage, cols: u16, rows: u16) -> Option<Image> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(pixels.clone())
            .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
            .ok()?;
        self.next_id += 1;
        let id = self.next_id;
        let encoded = base64::engine::general_purpose::STANDARD.encode(png);
        let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(CHUNK).collect();
        for (i, chunk) in chunks.iter().enumerate() {
            let more = u8::from(i + 1 < chunks.len());
            let _ = if i == 0 {
                write!(self.pending, "\x1b_Ga=t,f=100,i={id},q=2,m={more};")
            } else {
                write!(self.pending, "\x1b_Gm={more};")
            };
            self.pending.extend_from_slice(chunk);
            self.pending.extend_from_slice(b"\x1b\\");
        }
        Some(Image { id, cols, rows })
    }

    /// Frees an uploaded image and its placements.
    pub fn free(&mut self, image: Image) {
        let _ = write!(self.pending, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", image.id);
        self.placed.retain(|(id, _)| *id != image.id);
    }

    /// Forgets what's on screen, so the next flush places everything again.
    pub fn reset(&mut self) {
        self.placed.clear();
        self.pending.extend_from_slice(b"\x1b_Ga=d,d=a,q=2\x1b\\");
    }

    /// Sends queued uploads, then makes `wanted` the images on screen.
    pub fn flush(&mut self, out: &mut impl Write, wanted: &[(Image, Rect)]) -> io::Result<()> {
        let wanted: Vec<(u32, Rect)> = wanted.iter().map(|(image, rect)| (image.id, *rect)).collect();
        if wanted != self.placed {
            // Placements outlive the text drawn over them, so the old ones go first.
            self.pending.extend_from_slice(b"\x1b_Ga=d,d=a,q=2\x1b\\");
            self.pending.extend_from_slice(b"\x1b7");
            for (id, rect) in &wanted {
                let _ = write!(
                    self.pending,
                    "\x1b[{};{}H\x1b_Ga=p,i={id},c={},r={},C=1,q=2\x1b\\",
                    rect.y + 1,
                    rect.x + 1,
                    rect.width,
                    rect.height
                );
            }
            self.pending.extend_from_slice(b"\x1b8");
            self.placed = wanted;
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        out.write_all(&self.pending)?;
        self.pending.clear();
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flushed(kitty: &mut Kitty, wanted: &[(Image, Rect)]) -> String {
        let mut out = Vec::new();
        kitty.flush(&mut out, wanted).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn uploads_go_out_before_the_placement_and_nothing_goes_twice() {
        let mut kitty = Kitty::default();
        let image = kitty.upload(&RgbaImage::new(4, 4), 2, 1).unwrap();
        let rect = Rect::new(3, 5, 2, 1);
        let first = flushed(&mut kitty, &[(image, rect)]);
        let (upload, place) = (
            first.find("a=t,f=100,i=1").unwrap(),
            first.find("\x1b[6;4H\x1b_Ga=p,i=1,c=2,r=1").unwrap(),
        );
        assert!(upload < place);
        assert_eq!(flushed(&mut kitty, &[(image, rect)]), "");
    }

    #[test]
    fn placements_are_redrawn_once_the_screen_is_reset() {
        let mut kitty = Kitty::default();
        let image = kitty.upload(&RgbaImage::new(4, 4), 2, 1).unwrap();
        let wanted = [(image, Rect::new(0, 0, 2, 1))];
        flushed(&mut kitty, &wanted);
        kitty.reset();
        assert!(flushed(&mut kitty, &wanted).contains("a=p,i=1"));
    }

    #[test]
    fn no_placeholder_glyphs_are_written() {
        let mut kitty = Kitty::default();
        let image = kitty.upload(&RgbaImage::new(64, 64), 4, 4).unwrap();
        assert!(!flushed(&mut kitty, &[(image, Rect::new(0, 0, 4, 4))]).contains('\u{10EEEE}'));
    }
}
