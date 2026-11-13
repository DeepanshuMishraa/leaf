//! Which theme leaf draws in, and following it as it changes: `LEAF_THEME`,
//! then Rex's saved theme, then the terminal's own colors.

use std::time::{Duration, Instant};

use crate::rextheme;
use crate::theme::Theme;

/// How often a theme that can change is checked again.
pub const EVERY: Duration = Duration::from_secs(1);

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
}

/// Where the colors came from, which says whether and how they're rechecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// `LEAF_THEME`, or a terminal that won't say when its theme changes.
    Fixed,
    /// Rex's saved theme, read again when Rex's or macOS's settings change.
    Rex {
        stamp: rextheme::Stamp,
        checked: Instant,
    },
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
        let stamp = rextheme::stamp();
        if let Some(theme) = rextheme::active() {
            let checked = Instant::now();
            return (Scheme::Rex { stamp, checked }, Colors::of(theme));
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

    /// Checks again once `EVERY` has passed; the new colors when the theme changed.
    pub fn refresh(&mut self) -> Option<Colors> {
        let checked = match self {
            Scheme::Fixed => return None,
            Scheme::Rex { checked, .. } | Scheme::Followed { checked, .. } => checked,
        };
        if checked.elapsed() < EVERY {
            return None;
        }
        *checked = Instant::now();
        match self {
            Scheme::Fixed => None,
            Scheme::Rex { stamp, .. } => {
                let now = rextheme::stamp();
                if now == *stamp {
                    return None;
                }
                *stamp = now;
                rextheme::active().map(Colors::of)
            }
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

    /// How long an event loop may sleep before the scheme is due again.
    pub fn due(self) -> Option<Duration> {
        match self {
            Scheme::Fixed => None,
            Scheme::Rex { checked, .. } | Scheme::Followed { checked, .. } => {
                Some(EVERY.saturating_sub(checked.elapsed()))
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
            stamp: rextheme::stamp(),
            checked: Instant::now(),
        };
        assert!(rex.due().is_some_and(|wait| wait <= EVERY));
        assert_eq!(rex.refresh(), None);
    }
}
