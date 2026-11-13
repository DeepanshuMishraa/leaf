//! A terminal theme's own colors, which diff tints are mixed from so added
//! and removed lines take the theme's green and red instead of fixed ones.

use crate::mono::Rgb;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub background: Rgb,
    /// ANSI 1, 2 and 4.
    pub red: Rgb,
    pub green: Rgb,
    pub blue: Rgb,
    /// The theme's own accent: its cursor color, or its blue when it gives none.
    pub accent: Rgb,
    /// ANSI 5, or the blue when the theme gives none.
    pub magenta: Rgb,
}

impl Theme {
    /// Asks the terminal for its background (OSC 11) and ANSI colors (OSC 4);
    /// `None` when it won't say all of them.
    #[cfg(not(test))]
    pub fn query() -> Option<Self> {
        use crate::viewer::{query_color, query_palette};
        let blue = query_palette(4)?;
        Some(Theme {
            background: query_color(11)?,
            red: query_palette(1)?,
            green: query_palette(2)?,
            blue,
            // Not every terminal answers for its cursor (OSC 12).
            accent: query_color(12).unwrap_or(blue),
            magenta: query_palette(5).unwrap_or(blue),
        })
    }

    #[cfg(test)]
    pub fn query() -> Option<Self> {
        None
    }

    #[cfg_attr(test, allow(dead_code))]
    pub fn is_light(self) -> bool {
        is_light(self.background)
    }
}

/// Whether a background color is light enough for dark text.
pub fn is_light([r, g, b]: Rgb) -> bool {
    (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000 > 140
}

/// `top` laid over `bottom` at `amount` (0 to 1).
pub fn mix(bottom: Rgb, top: Rgb, amount: f32) -> Rgb {
    let channel =
        |b: u8, t: u8| (f32::from(b) + (f32::from(t) - f32::from(b)) * amount).round() as u8;
    [
        channel(bottom[0], top[0]),
        channel(bottom[1], top[1]),
        channel(bottom[2], top[2]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_moves_toward_the_top_color() {
        assert_eq!(mix([0, 0, 0], [200, 100, 50], 0.0), [0, 0, 0]);
        assert_eq!(mix([0, 0, 0], [200, 100, 50], 1.0), [200, 100, 50]);
        assert_eq!(mix([255, 255, 255], [0, 128, 0], 0.5), [128, 192, 128]);
        assert!(is_light([255, 255, 255]));
        assert!(!is_light([30, 30, 30]));
    }
}
