//! Which theme leaf draws in, and following it as it changes: `LEAF_THEME`,
//! then Rex's theme, then the terminal's own colors.

use std::time::{Duration, Instant};

use ratatui::style::Color;

use crate::mono::Rgb;
use crate::rextheme;
use crate::theme::{self, Theme};

/// How often a terminal's theme, or the UI setting, is checked again.
pub const EVERY: Duration = Duration::from_secs(1);
/// How often Rex's theme is read again: a cheap call to cfprefsd, often
/// enough that the dither follows a switch along with the rest of Rex.
pub const REX_EVERY: Duration = Duration::from_millis(250);

/// What's known of the theme: its colors when it gave them, and whether it's light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub theme: Option<Theme>,
    pub light: bool,
}

impl Colors {
    /// Nothing known: a dark theme is assumed.
    pub const DARK: Colors = Colors {
        theme: None,
        light: false,
    };

    fn of(theme: Theme) -> Self {
        Colors {
            theme: Some(theme),
            light: theme.is_light(),
        }
    }

    /// Blue from the active palette, with ANSI colors when it is unknown.
    pub fn heading_color(self) -> Color {
        self.readable(|theme| theme.blue, Color::Blue, Color::LightBlue)
    }

    /// A palette slot made readable on the background: mixed toward the text
    /// color when it is as light (or dark) as the background. Without the
    /// palette, the ANSI color for a light or a dark background.
    pub fn readable(self, slot: fn(Theme) -> Rgb, on_light: Color, on_dark: Color) -> Color {
        match self.theme {
            Some(theme) => {
                // A standard ANSI color can be faint on a background of its own lightness.
                let color = slot(theme);
                let [r, g, b] = if theme::is_light(color) == self.light {
                    let ink = if self.light { [0; 3] } else { [255; 3] };
                    theme::mix(color, ink, 0.4)
                } else {
                    color
                };
                Color::Rgb(r, g, b)
            }
            None if self.light => on_light,
            None => on_dark,
        }
    }
}

#[cfg(test)]
pub(crate) const HEADING_COLOR_CASES: [(Colors, Color); 2] = [
    (Colors::DARK, Color::LightBlue),
    (
        Colors {
            theme: Some(Theme {
                background: [16; 3],
                blue: [90, 170, 230],
                red: [160, 35, 55],
                green: [35, 146, 77],
                accent: [100, 70, 140],
                magenta: [100, 70, 140],
            }),
            light: false,
        },
        Color::Rgb(90, 170, 230),
    ),
];

/// Where the colors came from, which says whether and how they're rechecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// `LEAF_THEME`, or a terminal that won't say when its theme changes.
    Fixed,
    /// Rex's theme, read again every `REX_EVERY`.
    Rex { checked: Instant },
    /// The terminal reports its light or dark theme (Ghostty does), so it is
    /// asked again every `EVERY`, and its colors read again on a switch.
    Followed { light: bool, checked: Instant },
}

impl Scheme {
    /// `LEAF_THEME` first, then Rex's theme, then the terminal's colors, then
    /// its light or dark theme or background; dark when none say. Asks the
    /// terminal, so call it once the screen is in raw mode.
    #[cfg(not(test))]
    pub fn detect() -> (Self, Colors) {
        use std::io::IsTerminal;

        // The setting holds even when stdin isn't a terminal to ask.
        if let Some(light) = theme_hint(std::env::var("LEAF_THEME").ok().as_deref()) {
            return (Scheme::Fixed, Colors { theme: None, light });
        }
        if let Some(theme) = rextheme::active() {
            let checked = Instant::now();
            return (Scheme::Rex { checked }, Colors::of(theme));
        }
        if !std::io::stdin().is_terminal() {
            return (Scheme::Fixed, Colors::DARK);
        }
        let theme = Theme::query();
        if let Some(light) = crate::viewer::query_color_scheme() {
            let checked = Instant::now();
            return (Scheme::Followed { light, checked }, Colors { theme, light });
        }
        let light = theme.map_or_else(
            || crate::viewer::query_color(11).is_some_and(crate::theme::is_light),
            Theme::is_light,
        );
        (Scheme::Fixed, Colors { theme, light })
    }

    #[cfg(test)]
    pub fn detect() -> (Self, Colors) {
        (Scheme::Fixed, Colors::DARK)
    }

    /// How long this scheme waits between checks.
    fn every(self) -> Duration {
        match self {
            Scheme::Rex { .. } => REX_EVERY,
            Scheme::Fixed | Scheme::Followed { .. } => EVERY,
        }
    }

    /// Checks again once its wait has passed: the colors now for Rex, which
    /// the caller compares; for a terminal, new colors only on a switch.
    pub fn refresh(&mut self) -> Option<Colors> {
        let every = self.every();
        let checked = match self {
            Scheme::Fixed => return None,
            Scheme::Rex { checked } | Scheme::Followed { checked, .. } => checked,
        };
        if checked.elapsed() < every {
            return None;
        }
        *checked = Instant::now();
        match self {
            Scheme::Fixed => None,
            Scheme::Rex { .. } => rextheme::active().map(Colors::of),
            Scheme::Followed { light, .. } => match crate::viewer::query_color_scheme() {
                Some(now) if now != *light => {
                    *light = now;
                    Some(Colors {
                        theme: Theme::query(),
                        light: now,
                    })
                }
                _ => None,
            },
        }
    }

    /// Whether checking again asks the terminal, which reads its reply from
    /// stdin and could swallow a key typed meanwhile. Rex's theme comes from
    /// cfprefsd, which is safe at any moment.
    pub fn asks_terminal(self) -> bool {
        matches!(self, Scheme::Followed { .. })
    }

    /// How long an event loop may sleep before the scheme is due again.
    pub fn due(self) -> Option<Duration> {
        match self {
            Scheme::Fixed => None,
            Scheme::Rex { checked } | Scheme::Followed { checked, .. } => {
                Some(self.every().saturating_sub(checked.elapsed()))
            }
        }
    }
}

/// `LEAF_THEME=light` or `dark` sets the theme outright.
fn theme_hint(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "light" => Some(true),
        "dark" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_set_the_theme_and_schemes_wait_their_turn() {
        assert_eq!(theme_hint(Some("Light")), Some(true));
        assert_eq!(theme_hint(Some("dark")), Some(false));
        assert_eq!(theme_hint(Some("solarized")), None);
        assert_eq!(theme_hint(None), None);
        assert_eq!(Scheme::Fixed.due(), None);
        let mut fixed = Scheme::Fixed;
        assert_eq!(fixed.refresh(), None);
        let mut followed = Scheme::Followed {
            light: true,
            checked: Instant::now(),
        };
        assert!(followed.due().is_some_and(|wait| wait <= EVERY));
        // Not due yet, so the terminal isn't asked.
        assert_eq!(followed.refresh(), None);
        let mut rex = Scheme::Rex {
            checked: Instant::now(),
        };
        assert!(rex.due().is_some_and(|wait| wait <= REX_EVERY));
        assert_eq!(rex.refresh(), None);
    }
}
