//! Kitty graphics drawn as direct placements.
//!
//! ratatui-image draws kitty images through unicode placeholders, which some
//! terminals (Den among them) show as glyphs around and through the image.
//! Placing images at the cursor needs nothing written into the cells.

use std::io::{self, Write};
use std::time::{Duration, Instant};

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
    /// Images with a higher layer are drawn over those below.
    pub layer: i32,
}

#[derive(Default)]
pub struct Kitty {
    next_id: u32,
    /// Uploads and deletions waiting for the next flush.
    pending: Vec<u8>,
    placed: Vec<(u32, Rect)>,
    /// Images to drop once what replaces them is on screen.
    freed: Vec<u32>,
    /// Images replaced on screen, left under their successor until it has
    /// had time to show, and when each was replaced.
    retired: Vec<(u32, Instant)>,
}

/// How long a replaced image stays under its successor. A terminal may draw
/// nothing for a new image until it has turned it into a texture, so taking
/// the old one away at once shows a blink on every frame of a drag.
const LINGER: Duration = Duration::from_millis(250);

impl Kitty {
    /// Queues `pixels` for upload, to be drawn over `cols` × `rows` cells in `layer`.
    pub fn upload(
        &mut self,
        pixels: &RgbaImage,
        cols: u16,
        rows: u16,
        layer: i32,
    ) -> Option<Image> {
        use image::ImageEncoder;
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        let mut png = Vec::new();
        // Fast compression: ink is redrawn while the mouse moves.
        PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive)
            .write_image(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
                image::ExtendedColorType::Rgba8,
            )
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
        Some(Image {
            id,
            cols,
            rows,
            layer,
        })
    }

    /// Frees an uploaded image and its placements at the next flush, after
    /// whatever replaces it is placed, so the screen never shows a gap.
    pub fn free(&mut self, image: Image) {
        self.freed.push(image.id);
    }

    /// Frees an image a successor replaces on screen, leaving it in place
    /// beneath for a moment. The successor must sit in a higher layer.
    pub fn retire(&mut self, image: Image) {
        self.retired.push((image.id, Instant::now()));
    }

    /// Forgets what's on screen, so the next flush places everything again.
    pub fn reset(&mut self) {
        self.placed.clear();
        self.pending.extend_from_slice(b"\x1b_Ga=d,d=a,q=2\x1b\\");
        for (id, _) in self.retired.drain(..) {
            let _ = write!(self.pending, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
    }

    /// Sends queued uploads, then makes `wanted` the images on screen.
    /// Called with nothing new, it drops replaced images whose time is up.
    pub fn flush(&mut self, out: &mut impl Write, wanted: &[(Image, Rect)]) -> io::Result<()> {
        self.flush_at(out, wanted, Instant::now())
    }

    fn flush_at(
        &mut self,
        out: &mut impl Write,
        wanted: &[(Image, Rect)],
        now: Instant,
    ) -> io::Result<()> {
        let ids: Vec<(u32, Rect)> = wanted
            .iter()
            .map(|(image, rect)| (image.id, *rect))
            .collect();
        if ids != self.placed {
            // New placements go up before old ones come down; deleting first
            // leaves the screen empty while a large upload is still arriving,
            // which shows as a flicker. Placement id 1 makes re-placing an
            // image move it rather than draw it twice.
            self.pending.extend_from_slice(b"\x1b7");
            for (image, rect) in wanted {
                let _ = write!(
                    self.pending,
                    "\x1b[{};{}H\x1b_Ga=p,i={},c={},r={},z={},C=1,p=1,q=2\x1b\\",
                    rect.y + 1,
                    rect.x + 1,
                    image.id,
                    rect.width,
                    rect.height,
                    image.layer
                );
            }
            self.pending.extend_from_slice(b"\x1b8");
            for (id, _) in &self.placed {
                let leaving = !ids.iter().any(|(kept, _)| kept == id)
                    && !self.freed.contains(id)
                    && !self.retired.iter().any(|(old, _)| old == id);
                if leaving {
                    let _ = write!(self.pending, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\");
                }
            }
            self.placed = ids;
        }
        for id in self.freed.drain(..) {
            let _ = write!(self.pending, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
        }
        // With nothing left to cover them, replaced images go at once.
        let everything = wanted.is_empty();
        let pending = &mut self.pending;
        self.retired.retain(|(id, at)| {
            let keep = !everything && now.duration_since(*at) < LINGER;
            if !keep {
                let _ = write!(pending, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
            }
            keep
        });
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
        let image = kitty.upload(&RgbaImage::new(4, 4), 2, 1, 0).unwrap();
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
        let image = kitty.upload(&RgbaImage::new(4, 4), 2, 1, 0).unwrap();
        let wanted = [(image, Rect::new(0, 0, 2, 1))];
        flushed(&mut kitty, &wanted);
        kitty.reset();
        assert!(flushed(&mut kitty, &wanted).contains("a=p,i=1"));
    }

    #[test]
    fn a_replaced_image_is_dropped_only_after_its_successor_is_placed() {
        let mut kitty = Kitty::default();
        let rect = Rect::new(0, 0, 4, 4);
        let old = kitty.upload(&RgbaImage::new(8, 8), 4, 4, 0).unwrap();
        flushed(&mut kitty, &[(old, rect)]);
        kitty.free(old);
        let new = kitty.upload(&RgbaImage::new(8, 8), 4, 4, 0).unwrap();
        let out = flushed(&mut kitty, &[(new, rect)]);
        let upload = out.find("a=t,f=100,i=2").unwrap();
        let place = out.find("a=p,i=2").unwrap();
        let drop = out.find("a=d,d=I,i=1").unwrap();
        assert!(upload < place && place < drop);
        assert!(!out.contains("d=a"));
    }

    #[test]
    fn a_retired_image_stays_under_its_successor_until_its_time_is_up() {
        let mut kitty = Kitty::default();
        let rect = Rect::new(0, 0, 4, 4);
        let old = kitty.upload(&RgbaImage::new(8, 8), 4, 4, 0).unwrap();
        flushed(&mut kitty, &[(old, rect)]);
        kitty.retire(old);
        let new = kitty.upload(&RgbaImage::new(8, 8), 4, 4, 1).unwrap();
        let out = flushed(&mut kitty, &[(new, rect)]);
        assert!(out.contains("a=p,i=2"));
        assert!(
            !out.contains("i=1,"),
            "the old image is left alone: {out:?}"
        );
        let mut later = Vec::new();
        kitty
            .flush_at(&mut later, &[(new, rect)], Instant::now() + LINGER)
            .unwrap();
        assert_eq!(
            String::from_utf8(later).unwrap(),
            "\x1b_Ga=d,d=I,i=1,q=2\x1b\\"
        );
    }

    #[test]
    fn retired_images_go_at_once_when_nothing_is_shown() {
        let mut kitty = Kitty::default();
        let rect = Rect::new(0, 0, 4, 4);
        let old = kitty.upload(&RgbaImage::new(8, 8), 4, 4, 0).unwrap();
        flushed(&mut kitty, &[(old, rect)]);
        kitty.retire(old);
        assert!(flushed(&mut kitty, &[]).contains("a=d,d=I,i=1"));
    }

    #[test]
    fn no_placeholder_glyphs_are_written() {
        let mut kitty = Kitty::default();
        let image = kitty.upload(&RgbaImage::new(64, 64), 4, 4, 0).unwrap();
        assert!(!flushed(&mut kitty, &[(image, Rect::new(0, 0, 4, 4))]).contains('\u{10EEEE}'));
    }
}
