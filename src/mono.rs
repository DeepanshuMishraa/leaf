//! Painting images in the terminal's text color.

use image::{Rgba, RgbaImage};

pub type Rgb = [u8; 3];

/// Ink fainter than this is scan noise or JPEG haze on the paper, and is dropped.
const NOISE: f32 = 0.08;

fn luminance(p: &Rgba<u8>) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

/// Repaints an image as `ink` on nothing: every pixel takes the ink color,
/// and how much ink it holds becomes its opacity. The paper turns fully
/// transparent, so the drawing sits on the terminal like its text. Mostly
/// light images (diagrams on white) treat their dark lines as ink; mostly dark
/// images (photos) treat their light parts as ink. Contrast is stretched so
/// faint scans still read.
pub fn paint(image: &RgbaImage, ink: Rgb) -> RgbaImage {
    let levels: Vec<f32> = image.pixels().map(luminance).collect();
    let min = levels.iter().copied().fold(1.0_f32, f32::min);
    let max = levels.iter().copied().fold(0.0_f32, f32::max);
    let range = max - min;
    let ink_is_dark = levels.iter().sum::<f32>() / levels.len().max(1) as f32 > 0.5;
    let mut out = RgbaImage::new(image.width(), image.height());
    for (pixel, level) in out.pixels_mut().zip(levels) {
        // A flat image is all paper.
        let amount = if range == 0.0 {
            0.0
        } else {
            let level = (level - min) / range;
            if ink_is_dark { 1.0 - level } else { level }
        };
        let alpha = ((amount - NOISE) / (1.0 - NOISE)).max(0.0);
        *pixel = Rgba([ink[0], ink[1], ink[2], (alpha * 255.0).round() as u8]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_lines_on_white_become_opaque_ink_on_transparent_paper() {
        let mut image = RgbaImage::from_pixel(4, 1, Rgba([255, 255, 255, 255]));
        image.put_pixel(0, 0, Rgba([0, 0, 0, 255]));
        let painted = paint(&image, [10, 20, 30]);
        assert_eq!(painted.get_pixel(0, 0).0, [10, 20, 30, 255]);
        assert_eq!(painted.get_pixel(1, 0).0[3], 0);
    }

    #[test]
    fn a_flat_image_is_all_paper() {
        let image = RgbaImage::from_pixel(2, 2, Rgba([90, 90, 90, 255]));
        assert!(paint(&image, [1, 2, 3]).pixels().all(|p| p[3] == 0));
    }
}
