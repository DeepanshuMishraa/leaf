//! How a running leaf's screens look: the theme's colors, followed as the
//! theme changes, and whether the dither UI is on, picked up when `leaf ui`
//! or Rex's palette switches it in another process.

use std::time::{Duration, Instant};

use crate::art::Ink;
use crate::progress::{Store, Ui};
use crate::scheme::{self, Colors, Scheme};

pub struct Look {
    scheme: Scheme,
    colors: Colors,
    ui: Ui,
    /// Where the UI setting is read again from; `None` keeps the one read first.
    store: Option<Store>,
    checked: Instant,
}

impl Look {
    /// The theme and the saved UI. Asks the terminal, so call it once the
    /// screen is in raw mode.
    pub fn detect() -> Self {
        let (scheme, colors) = Scheme::detect();
        let store = settings();
        let ui = store
            .as_ref()
            .and_then(|store| store.load_ui().ok())
            .unwrap_or_default();
        Look {
            scheme,
            colors,
            ui,
            store,
            checked: Instant::now(),
        }
    }

    pub fn colors(&self) -> Colors {
        self.colors
    }

    /// The dither's tints, or `None` when the classic UI is on.
    pub fn ink(&self) -> Option<Ink> {
        match self.ui {
            Ui::Dither => Some(Ink::of_colors(self.colors)),
            Ui::Classic => None,
        }
    }

    /// Reads the UI setting again once `scheme::EVERY` has passed, and the
    /// theme too when `idle`: following a terminal's theme reads its reply
    /// from stdin, which would swallow a key typed meanwhile. True when
    /// either changed, so the screen is drawn again.
    pub fn refresh(&mut self, idle: bool) -> bool {
        let mut changed = false;
        if idle && let Some(colors) = self.scheme.refresh() {
            changed |= colors != self.colors;
            self.colors = colors;
        }
        if self.checked.elapsed() >= scheme::EVERY {
            self.checked = Instant::now();
            if let Some(ui) = self.store.as_ref().and_then(|s| s.load_ui().ok()) {
                changed |= ui != self.ui;
                self.ui = ui;
            }
        }
        changed
    }

    /// How long an event loop may sleep before something here is due again.
    pub fn due(&self) -> Duration {
        let setting = scheme::EVERY.saturating_sub(self.checked.elapsed());
        self.scheme
            .due()
            .map_or(setting, |theme| theme.min(setting))
    }
}

/// The settings store, read-only in spirit; tests never touch the real one.
#[cfg(not(test))]
fn settings() -> Option<Store> {
    Store::open(&crate::progress::default_path()).ok()
}

#[cfg(test)]
fn settings() -> Option<Store> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dither_draws_until_classic_is_picked() {
        let mut look = Look::detect();
        assert!(look.ink().is_some());
        look.ui = Ui::Classic;
        assert!(look.ink().is_none());
        assert!(look.due() <= scheme::EVERY);
        assert!(!look.refresh(true), "nothing changed");
    }
}
