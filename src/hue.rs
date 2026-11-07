//! The colors highlights and drawings come in.

use ratatui::style::Color;

use crate::mono::Rgb;

/// A marker color, picked with the number keys. Highlights use a light
/// shade that dark ink reads on in light and dark themes; drawings use a
/// deeper one that shows on either background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Hue {
    #[default]
    Yellow,
    Green,
    Blue,
    Pink,
    Purple,
    Red,
}

impl Hue {
    pub const ALL: [Hue; 6] = [
        Hue::Yellow,
        Hue::Green,
        Hue::Blue,
        Hue::Pink,
        Hue::Purple,
        Hue::Red,
    ];

    /// The hue a number key picks: `1` is yellow, `6` red.
    pub fn of_key(c: char) -> Option<Hue> {
        let n = c.to_digit(10)? as usize;
        Hue::ALL.get(n.checked_sub(1)?).copied()
    }

    pub fn name(self) -> &'static str {
        match self {
            Hue::Yellow => "yellow",
            Hue::Green => "green",
            Hue::Blue => "blue",
            Hue::Pink => "pink",
            Hue::Purple => "purple",
            Hue::Red => "red",
        }
    }

    pub fn parse(name: &str) -> Option<Hue> {
        Hue::ALL.into_iter().find(|h| h.name() == name)
    }

    /// Behind highlighted text.
    pub fn marker(self) -> Color {
        let [r, g, b] = match self {
            Hue::Yellow => [250, 214, 64],
            Hue::Green => [150, 222, 120],
            Hue::Blue => [140, 190, 255],
            Hue::Pink => [255, 160, 200],
            Hue::Purple => [200, 165, 255],
            Hue::Red => [255, 135, 125],
        };
        Color::Rgb(r, g, b)
    }

    /// The pen's ink.
    pub fn ink(self) -> Rgb {
        match self {
            Hue::Yellow => [235, 180, 0],
            Hue::Green => [40, 175, 80],
            Hue::Blue => [50, 120, 235],
            Hue::Pink => [230, 70, 150],
            Hue::Purple => [145, 90, 225],
            Hue::Red => [225, 55, 50],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_keys_pick_hues_and_names_round_trip() {
        assert_eq!(Hue::of_key('1'), Some(Hue::Yellow));
        assert_eq!(Hue::of_key('6'), Some(Hue::Red));
        assert_eq!(Hue::of_key('0'), None);
        assert_eq!(Hue::of_key('7'), None);
        for hue in Hue::ALL {
            assert_eq!(Hue::parse(hue.name()), Some(hue));
        }
        assert_eq!(Hue::parse("mauve"), None);
    }
}
