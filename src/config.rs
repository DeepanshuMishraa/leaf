use std::io;
use std::path::PathBuf;

use serde::Deserialize;

use crate::boardkeys::BoardAction;
use crate::diffkeys::{CommentsAction, ConfirmAction, DiffAction, OverviewAction, VerdictAction};
use crate::diffview::Layout;
use crate::keymap::{self, Keymap};
use crate::outline::{self, OutlineAction};
use crate::plankeys::{PlanAction, PlanNotesAction, PlanQuitAction};
use crate::prkeys::{PrAction, PrListAction};
use crate::readerkeys::{BookmarksAction, InkAction, ReaderAction};

pub const DEFAULT_MAX_WIDTH: u16 = 76;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub reader: ReaderConfig,
    pub diff: DiffConfig,
    pub outline: OutlineConfig,
    pub keys: KeysConfig,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiffConfig {
    /// The layout a diff opens in. Left out, it opens in the one last used;
    /// `--inline` on the command line wins over either.
    pub layout: Option<Layout>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutlineConfig {
    /// The sidebar's width when it first opens, in columns.
    pub width: u16,
}

impl Default for OutlineConfig {
    fn default() -> Self {
        Self {
            width: outline::SIDEBAR_COLUMNS,
        }
    }
}

/// Declares `KeysConfig`: one `[keys.<view>]` table for each view's actions.
macro_rules! keys_config {
    ($($field:ident: $action:ty),+ $(,)?) => {
        /// `[keys.<view>]`: each names an action and the key, or list of keys,
        /// that does it. Actions left out keep their default keys.
        #[derive(Debug, Default, Clone, PartialEq, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct KeysConfig {
            $(pub $field: Keymap<$action>,)+
        }

        impl KeysConfig {
            /// Every table with every action at its default keys: `leaf keys`.
            pub fn defaults_toml() -> String {
                [$(keymap::defaults_toml::<$action>()),+].join("\n")
            }

            /// The `[keys.<view>]` names, each with the field it fills.
            #[cfg(test)]
            fn views() -> Vec<(&'static str, &'static str)> {
                vec![$((stringify!($field), <$action as keymap::Action>::VIEW)),+]
            }

            #[cfg(test)]
            fn check_tables() {
                $(keymap::check::<$action>();)+
            }
        }
    };
}

keys_config! {
    reader: ReaderAction,
    ink: InkAction,
    bookmarks: BookmarksAction,
    outline: OutlineAction,
    diff: DiffAction,
    diff_comments: CommentsAction,
    diff_overview: OverviewAction,
    confirm_post: ConfirmAction,
    verdict: VerdictAction,
    plan: PlanAction,
    plan_notes: PlanNotesAction,
    plan_quit: PlanQuitAction,
    board: BoardAction,
    pr_list: PrListAction,
    pr: PrAction,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderConfig {
    pub max_width: u16,
    pub colorful_markdown: bool,
    pub hide_link_urls: bool,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
            colorful_markdown: false,
            hide_link_urls: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Could not load settings in {path}:\n{cause}", path = .path.display())]
pub struct ConfigFailed {
    pub path: PathBuf,
    pub cause: String,
}

/// Where `config.toml` is read from: `$XDG_CONFIG_HOME/leaf`, else `~/.config/leaf`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .map(|home| home.join("leaf").join("config.toml"))
}

pub fn load() -> Result<Config, ConfigFailed> {
    let Some(path) = default_path() else {
        return Ok(Config::default());
    };
    let failed = |cause: String| ConfigFailed {
        path: path.clone(),
        cause: cause.trim_end().to_owned(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(failed(error.to_string())),
    };
    let config: Config = toml::from_str(&text).map_err(|error| failed(error.to_string()))?;
    if config.reader.max_width == 0 {
        return Err(failed(
            "reader.max_width must be an integer from 1 through 65535".into(),
        ));
    }
    if !(outline::SIDEBAR_MIN..=outline::SIDEBAR_MAX).contains(&config.outline.width) {
        return Err(failed(format!(
            "outline.width must be from {} through {} columns; it is {}",
            outline::SIDEBAR_MIN,
            outline::SIDEBAR_MAX,
            config.outline.width
        )));
    }
    config.keys.check_overlaps().map_err(failed)?;
    Ok(config)
}

impl KeysConfig {
    /// Refuses keys two tables would both want at once. With a stroke picked,
    /// `[keys.ink]` hears a key before the reader, so a color sharing an ink
    /// key could never be picked there.
    pub fn check_overlaps(&self) -> Result<(), String> {
        use keymap::Action;
        let colors = ReaderAction::TABLE
            .iter()
            .filter(|(_, action, _)| action.hue().is_some());
        for ((ink_name, ink, _), what) in InkAction::TABLE.iter().zip(InkAction::ABOUT) {
            for (color_name, color, _) in colors.clone() {
                let theirs = self.reader.chords(*color);
                if let Some(chord) = self.ink.chords(*ink).iter().find(|c| theirs.contains(c)) {
                    return Err(format!(
                        "keys.ink.{ink_name} and keys.reader.{color_name} both use `{}`, so with a stroke picked that key would only {what}. Give one of them another key.",
                        chord.label(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::diffkeys::DiffAction;
    use crate::readerkeys::ReaderAction;

    #[test]
    fn every_keys_table_is_named_for_its_field_and_sound() {
        super::KeysConfig::check_tables();
        for (field, view) in super::KeysConfig::views() {
            assert_eq!(field, view);
        }
    }

    #[test]
    fn the_printed_defaults_read_back_as_the_defaults() {
        let printed = super::KeysConfig::defaults_toml();
        let config: Config = toml::from_str(&printed).unwrap();
        assert_eq!(config.keys, super::KeysConfig::default());
        assert!(config.keys.check_overlaps().is_ok());
    }

    #[test]
    fn a_color_on_an_ink_key_is_refused() {
        let config: Config = toml::from_str("[keys.reader]\ncolor_1 = \"h\"").unwrap();
        let error = config.keys.check_overlaps().unwrap_err();
        assert!(
            error.contains("keys.ink.left") && error.contains("keys.reader.color_1"),
            "{error}"
        );
        let moved: Config =
            toml::from_str("[keys.reader]\ncolor_1 = \"h\"\n\n[keys.ink]\nleft = \"left\"")
                .unwrap();
        assert!(moved.keys.check_overlaps().is_ok());
    }

    #[test]
    fn minor_settings_read() {
        let config: Config =
            toml::from_str("[diff]\nlayout = \"inline\"\n\n[outline]\nwidth = 40").unwrap();
        assert_eq!(config.diff.layout, Some(crate::diffview::Layout::Inline));
        assert_eq!(config.outline.width, 40);
        assert!(toml::from_str::<Config>("[diff]\nlayout = \"sideways\"").is_err());
    }

    #[test]
    fn keys_rebind_per_view() {
        let config: Config = toml::from_str(
            "[keys.reader]\nquit = [\"x\", \"ctrl+q\"]\n\n[keys.diff]\nnext_file = \"tab\"",
        )
        .unwrap();
        assert_eq!(config.keys.reader.label(ReaderAction::Quit), "x/^q");
        assert_eq!(config.keys.reader.label(ReaderAction::ScrollDown), "j/↓");
        assert_eq!(config.keys.diff.label(DiffAction::NextFile), "tab");
    }

    #[test]
    fn a_bad_binding_names_where_it_is() {
        let error = toml::from_str::<Config>("[keys.reader]\nquit = \"ctl+q\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("keys.reader.quit"), "{error}");
        let error = toml::from_str::<Config>("[keys.browser]\nquit = \"q\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("browser"), "{error}");
    }

    #[test]
    fn colorful_markdown_is_opt_in() {
        for (text, expected) in [
            ("", false),
            ("[reader]\nmax_width = 92", false),
            ("[reader]\ncolorful_markdown = false", false),
            ("[reader]\ncolorful_markdown = true", true),
        ] {
            let config: Config = toml::from_str(text).unwrap();
            assert_eq!(config.reader.colorful_markdown, expected, "{text:?}");
        }
    }

    #[test]
    fn hiding_link_urls_is_opt_in() {
        let default: Config = toml::from_str("[reader]\nmax_width = 92").unwrap();
        let hidden: Config =
            toml::from_str("[reader]\ncolorful_markdown = true\nhide_link_urls = true").unwrap();
        assert!(!default.reader.hide_link_urls);
        assert!(hidden.reader.hide_link_urls);
        assert!(hidden.reader.colorful_markdown);
    }
}
