use std::io;
use std::path::PathBuf;

use serde::Deserialize;

pub const DEFAULT_MAX_WIDTH: u16 = 76;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub reader: ReaderConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReaderConfig {
    pub max_width: u16,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            max_width: DEFAULT_MAX_WIDTH,
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
