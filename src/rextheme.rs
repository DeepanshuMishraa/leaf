//! Rex's theme, read from the Mac app's settings. Rex paints its theme in the
//! app, while the terminal leaf talks to answers color queries with built-in
//! defaults, so asking the terminal can't tell which theme is on. The
//! settings are read through cfprefsd, which has a switch the moment Rex
//! makes it; the plist on disk catches up only seconds later.

use serde_json::Value;

use crate::mono::Rgb;
use crate::theme::Theme;

/// Rex's preferences domain.
#[cfg(target_os = "macos")]
const DOMAIN: &str = "com.superlogical.rex";
/// Rex's built-in themes, inside the app.
const CATALOG: &str =
    "Contents/Resources/SuperUI_SuperDesignTokens.bundle/Contents/Resources/themes.json";
const APPS: [&str; 2] = ["Rex Beta.app", "Rex.app"];

/// Whether leaf runs in a Rex pane.
pub fn inside_rex() -> bool {
    std::env::var("REX_SESSION").is_ok_and(|s| !s.trim().is_empty())
}

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(std::path::PathBuf::from)
}

/// The theme Rex shows now. `None` outside Rex, on a host without Rex's
/// settings (a remote pane), or when they can't be read; the caller then
/// asks the terminal.
#[cfg(target_os = "macos")]
pub fn active() -> Option<Theme> {
    use crate::prefs::{self, Pref};

    if !inside_rex() {
        return None;
    }
    prefs::sync(DOMAIN);
    let text = |key: &str| prefs::read(DOMAIN, key).and_then(|v| v.text().map(str::to_owned));
    let light = match text("Workspace.appearance").as_deref() {
        Some("light") => true,
        Some("dark") => false,
        // Following macOS, whose AppleInterfaceStyle exists only in dark mode.
        _ => prefs::read_global("AppleInterfaceStyle")
            .as_ref()
            .and_then(Pref::text)
            .is_none_or(|style| style != "Dark"),
    };
    let id = text(if light {
        "Workspace.lightThemeID"
    } else {
        "Workspace.darkThemeID"
    })?;
    let id = id.as_str();
    let custom = prefs::read(DOMAIN, "Workspace.customThemes")
        .as_ref()
        .and_then(Pref::data)
        .and_then(|data| serde_json::from_slice::<Value>(data).ok());
    if let Some(theme) = custom.as_ref().and_then(|themes| custom_theme(themes, id)) {
        return Some(theme);
    }
    catalog()?.iter().find_map(|entry| catalog_theme(entry, id))
}

#[cfg(not(target_os = "macos"))]
pub fn active() -> Option<Theme> {
    None
}

/// The built-in themes, read once: they only change when Rex updates.
#[cfg(target_os = "macos")]
fn catalog() -> Option<&'static [Value]> {
    static CATALOG_THEMES: std::sync::OnceLock<Option<Vec<Value>>> = std::sync::OnceLock::new();
    CATALOG_THEMES
        .get_or_init(|| {
            let apps = ["/Applications".into()]
                .into_iter()
                .chain(home().map(|home| home.join("Applications")));
            let path = apps
                .flat_map(|dir: std::path::PathBuf| APPS.map(|app| dir.join(app).join(CATALOG)))
                .find(|path| path.is_file())?;
            let text = std::fs::read(path).ok()?;
            match serde_json::from_slice(&text).ok()? {
                Value::Array(entries) => Some(entries),
                _ => None,
            }
        })
        .as_deref()
}

/// A built-in theme: hex colors under `source`.
fn catalog_theme(entry: &Value, id: &str) -> Option<Theme> {
    if entry.get("id")?.as_str()? != id {
        return None;
    }
    let source = entry.get("source")?;
    let ansi = |index: usize| hex(source.get("ansi")?.get(index)?.as_str()?);
    let blue = ansi(4)?;
    Some(Theme {
        background: hex(source.get("background")?.as_str()?)?,
        red: ansi(1)?,
        green: ansi(2)?,
        blue,
        accent: source
            .get("cursor")
            .and_then(Value::as_str)
            .and_then(hex)
            .unwrap_or(blue),
        magenta: ansi(5).unwrap_or(blue),
    })
}

/// A theme the user made or imported, saved as `{red, green, blue,
/// colorSpace}` colors under `palette`, with hand-picked ANSI colors in
/// `paletteOverrides`.
fn custom_theme(themes: &Value, id: &str) -> Option<Theme> {
    let entry = themes
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))?;
    let palette = entry.get("palette")?;
    let ansi = |index: usize| {
        let overridden = palette
            .get("paletteOverrides")
            .and_then(|overrides| overrides.get(index.to_string()));
        stored(overridden.or_else(|| palette.get("ansi")?.get(index))?)
    };
    let blue = ansi(4)?;
    Some(Theme {
        background: stored(palette.get("background")?)?,
        red: ansi(1)?,
        green: ansi(2)?,
        blue,
        accent: palette.get("cursor").and_then(stored).unwrap_or(blue),
        magenta: ansi(5).unwrap_or(blue),
    })
}

/// `#RRGGBB` or `RRGGBB`.
fn hex(text: &str) -> Option<Rgb> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// A color Rex stored as 0–1 channels, in sRGB or Display P3.
fn stored(color: &Value) -> Option<Rgb> {
    let channel = |name: &str| color.get(name)?.as_f64();
    let rgb = [channel("red")?, channel("green")?, channel("blue")?];
    let p3 = color
        .get("colorSpace")
        .and_then(Value::as_str)
        .is_some_and(|space| space.to_ascii_lowercase().contains("p3"));
    let rgb = if p3 { p3_to_srgb(rgb) } else { rgb };
    Some(rgb.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8))
}

/// Display P3 to sRGB: both share sRGB's transfer curve, so decode, convert
/// the primaries, and encode again. Colors outside sRGB are clipped.
fn p3_to_srgb(rgb: [f64; 3]) -> [f64; 3] {
    let decode = |c: f64| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode = |c: f64| {
        let c = c.clamp(0.0, 1.0);
        if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    };
    let [r, g, b] = rgb.map(decode);
    [
        encode(1.224_940_2 * r - 0.224_940_4 * g),
        encode(-0.042_056_9 * r + 1.042_057_1 * g),
        encode(-0.019_637_6 * r - 0.078_636_1 * g + 1.098_273_5 * b),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_built_in_theme_is_found_by_id() {
        let entry = json!({
            "id": "charcoal-atelier",
            "source": {
                "background": "#161616",
                "ansi": ["#222222", "#d96a4d", "#9aab84", "#cfae7a", "#8aa0b4"]
            }
        });
        assert_eq!(catalog_theme(&entry, "other"), None);
        assert_eq!(
            catalog_theme(&entry, "charcoal-atelier"),
            Some(Theme {
                background: [0x16, 0x16, 0x16],
                red: [0xd9, 0x6a, 0x4d],
                green: [0x9a, 0xab, 0x84],
                blue: [0x8a, 0xa0, 0xb4],
                accent: [0x8a, 0xa0, 0xb4],
                magenta: [0x8a, 0xa0, 0xb4],
            })
        );
    }

    #[test]
    fn a_custom_theme_takes_its_overrides() {
        let gray = |v: f64| json!({ "red": v, "green": v, "blue": v, "colorSpace": "srgb" });
        let themes = json!([
            { "id": "mine", "palette": {
                "background": gray(1.0),
                "ansi": [gray(0.0), gray(0.2), gray(0.4), gray(0.6), gray(0.8)],
                "paletteOverrides": { "2": { "red": 0.0, "green": 1.0, "blue": 0.0 } }
            } }
        ]);
        assert_eq!(custom_theme(&themes, "missing"), None);
        assert_eq!(
            custom_theme(&themes, "mine"),
            Some(Theme {
                background: [255, 255, 255],
                red: [51, 51, 51],
                green: [0, 255, 0],
                blue: [204, 204, 204],
                accent: [204, 204, 204],
                magenta: [204, 204, 204],
            })
        );
    }

    #[test]
    fn colors_parse_from_hex_and_display_p3() {
        assert_eq!(hex("#0a0B0c"), Some([10, 11, 12]));
        assert_eq!(hex("0a0b0c"), Some([10, 11, 12]));
        assert_eq!(hex("#0a0b"), None);
        assert_eq!(hex("#zz0b0c"), None);
        // White and gray stay put; a P3 red is clipped to sRGB's red.
        let p3 = |r: f64, g: f64, b: f64| json!({ "red": r, "green": g, "blue": b, "colorSpace": "displayP3" });
        assert_eq!(stored(&p3(1.0, 1.0, 1.0)), Some([255, 255, 255]));
        assert_eq!(stored(&p3(1.0, 0.0, 0.0)), Some([255, 0, 0]));
        assert_eq!(stored(&json!({ "red": 0.5 })), None);
    }
}
