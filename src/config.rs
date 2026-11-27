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
    pub simulator: SimulatorConfig,
}

/// `[simulator]`: the iOS Simulator pane, on macOS. `leaf sim install`
/// downloads its models and turns it on; Rex's palette offers it only then.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SimulatorConfig {
    pub enabled: bool,
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

/// Sets `enabled = true` under `[simulator]` in leaf's config, keeping the
/// rest of the file as it was. Returns the file's path.
pub fn enable_simulator() -> Result<PathBuf, (PathBuf, io::Error)> {
    let path = default_path().ok_or_else(|| {
        (
            PathBuf::from("~/.config/leaf/config.toml"),
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither XDG_CONFIG_HOME nor HOME is set",
            ),
        )
    })?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err((path, error)),
    };
    let updated = with_simulator_enabled(&text);
    if updated != text {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| (path.clone(), error))?;
        }
        // Written beside and renamed over, so a failed write (a full disk)
        // leaves the old file whole.
        let partial = path.with_extension(format!("toml.{}.tmp", std::process::id()));
        let written = std::fs::write(&partial, updated)
            .and_then(|()| std::fs::File::open(&partial)?.sync_all())
            .and_then(|()| std::fs::rename(&partial, &path));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&partial);
            return Err((path, error));
        }
    }
    Ok(path)
}

/// `text` with `enabled = true` in its `[simulator]` table: an `enabled`
/// line there is replaced, else one is added to the table, else the table
/// is appended.
fn with_simulator_enabled(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let header = |line: &str| line.trim().starts_with('[');
    let Some(start) = lines
        .iter()
        .position(|line| table_name(line).as_deref() == Some("simulator"))
    else {
        let mut out = text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("[simulator]\nenabled = true\n");
        return out;
    };
    let end = lines[start + 1..]
        .iter()
        .position(|line| header(line))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    let key = |line: &str| line.split('=').next().map(str::trim) == Some("enabled");
    match (start + 1..end).find(|&index| key(lines[index])) {
        Some(index) => out[index] = "enabled = true".into(),
        None => out.insert(start + 1, "enabled = true".into()),
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// The table a `[name]` header line opens, however it's spaced, quoted or
/// commented (`[ simulator ]  # the pane`); None for any other line,
/// including `[[array]]` headers.
fn table_name(line: &str) -> Option<String> {
    let line = line.split('#').next()?.trim();
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') {
        return None;
    }
    let name = inner.trim();
    let unquoted = name
        .strip_prefix('"')
        .and_then(|name| name.strip_suffix('"'))
        .or_else(|| {
            name.strip_prefix('\'')
                .and_then(|name| name.strip_suffix('\''))
        })
        .unwrap_or(name);
    Some(unquoted.to_owned())
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
    use super::{Config, table_name, with_simulator_enabled};

    #[test]
    fn table_headers_are_read_however_written() {
        assert_eq!(table_name("[simulator]").as_deref(), Some("simulator"));
        assert_eq!(
            table_name("  [ simulator ]  # the pane").as_deref(),
            Some("simulator")
        );
        assert_eq!(table_name("['simulator']").as_deref(), Some("simulator"));
        assert_eq!(table_name("[keys.reader]").as_deref(), Some("keys.reader"));
        assert_eq!(table_name("[[simulator]]"), None);
        assert_eq!(table_name("enabled = true"), None);
    }

    #[test]
    fn enabling_the_simulator_edits_only_its_table() {
        assert_eq!(with_simulator_enabled(""), "[simulator]\nenabled = true\n");
        assert_eq!(
            with_simulator_enabled("[reader]\nmax_width = 90"),
            "[reader]\nmax_width = 90\n\n[simulator]\nenabled = true\n"
        );
        assert_eq!(
            with_simulator_enabled("[simulator]\nenabled = false\n[diff]\nlayout = \"inline\"\n"),
            "[simulator]\nenabled = true\n[diff]\nlayout = \"inline\"\n"
        );
        assert_eq!(
            with_simulator_enabled("[simulator]\n[keys.reader]\n"),
            "[simulator]\nenabled = true\n[keys.reader]\n"
        );
        // However the header is written, the one table is edited.
        for header in [
            "[ simulator ]",
            "[simulator]  # the pane",
            "[\"simulator\"]",
        ] {
            let text = format!("{header}\nenabled = false\n");
            let updated = with_simulator_enabled(&text);
            assert_eq!(updated, format!("{header}\nenabled = true\n"), "{header}");
            let parsed: Config = toml::from_str(&updated).expect("parses");
            assert!(parsed.simulator.enabled, "{header}");
        }
        let enabled: Config =
            toml::from_str(&with_simulator_enabled("[reader]\nmax_width = 90\n")).expect("parses");
        assert!(enabled.simulator.enabled);
        assert_eq!(enabled.reader.max_width, 90);
    }
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
