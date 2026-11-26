use std::io;
use std::path::PathBuf;

use serde::Deserialize;

use crate::diffkeys::DiffAction;
use crate::keymap::Keymap;
use crate::readerkeys::ReaderAction;

pub const DEFAULT_MAX_WIDTH: u16 = 76;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub reader: ReaderConfig,
    pub keys: KeysConfig,
}

/// `[keys.reader]` and `[keys.diff]`: each names an action and the key, or
/// list of keys, that does it. Actions left out keep their default keys.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeysConfig {
    pub reader: Keymap<ReaderAction>,
    pub diff: Keymap<DiffAction>,
}

#[derive(Debug, Deserialize)]
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

fn default_path() -> Option<PathBuf> {
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
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::Config;
    use crate::diffkeys::DiffAction;
    use crate::readerkeys::ReaderAction;

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
        let error = toml::from_str::<Config>("[keys.board]\nquit = \"q\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("board"), "{error}");
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
