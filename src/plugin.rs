//! Installs leaf as a Rex plugin: a Lua file beside Rex's config, and a marked
//! block in init.lua that loads it. The actions then show up in the command palette.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BEGIN: &str = "-- >>> leaf >>>";
const END: &str = "-- <<< leaf <<<";
const SOURCE: &str = include_str!("../lua/leaf/init.lua");

pub fn install() -> Result<String, String> {
    let bin =
        std::env::current_exe().map_err(|e| format!("could not find this leaf binary: {e}"))?;
    let config = config_path();
    let plugin_path = config
        .parent()
        .unwrap_or(Path::new("."))
        .join("plugins")
        .join("leaf")
        .join("init.lua");
    let previous = fs::read_to_string(&config).unwrap_or_default();
    let next = with_block(&previous, Some(&loader(&plugin_path, &bin)));
    write_if_changed(&plugin_path, SOURCE)?;
    write_if_changed(&config, &next)?;
    if let Err(error) = rex(&["config", "check", &config.display().to_string()]) {
        let _ = fs::write(&config, previous);
        return Err(error);
    }
    let reloaded = rex(&["config", "reload"]).is_ok();
    Ok(format!(
        "Leaf is in Rex's command palette{}. Press cmd+shift+p and type Leaf.\n  Leaf: Open Document… picks a file here or one read lately.\n  Select text, then @ (or Leaf: Ask Agent About Selection) sends it, with the lines around it, to the agent pane.\n  Leaf: Read Aloud / Pause plays the open document; Leaf: Voice — … picks who reads.\n  Leaf: Review Diff, Review Staged Changes and Review This Branch open the diff viewer.\nIn a session on a remote host, leaf is installed there with cargo the first time.",
        if reloaded {
            ""
        } else {
            " (rex config reload did not run; reload Rex's config)"
        }
    ))
}

pub fn uninstall() -> Result<String, String> {
    let config = config_path();
    let plugin_path = config
        .parent()
        .unwrap_or(Path::new("."))
        .join("plugins")
        .join("leaf")
        .join("init.lua");
    let previous = fs::read_to_string(&config).unwrap_or_default();
    let next = with_block(&previous, None);
    let changed = next != previous;
    if changed {
        write_if_changed(&config, &next)?;
        if let Err(error) = rex(&["config", "check", &config.display().to_string()]) {
            let _ = fs::write(&config, previous);
            return Err(error);
        }
        let _ = rex(&["config", "reload"]);
    }
    if plugin_path.is_file() {
        fs::remove_file(&plugin_path).map_err(|e| e.to_string())?;
    }
    Ok("Removed Leaf from the command palette.".into())
}

fn config_path() -> PathBuf {
    if let Some(path) = std::env::var_os("REX_CONFIG").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(home) = std::env::var_os("XDG_CONFIG_HOME").filter(|path| !path.is_empty()) {
        return PathBuf::from(home).join("rex").join("init.lua");
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".config")
        .join("rex")
        .join("init.lua")
}

fn loader(plugin_path: &Path, bin: &Path) -> String {
    let voices: Vec<String> = crate::edge::built_in()
        .iter()
        .map(|voice| {
            format!(
                "{{ name = {}, short = {}, about = {} }}",
                lua_string(&voice.name),
                lua_string(voice.short()),
                lua_string(&format!(
                    "{}, {}",
                    voice.language,
                    voice.gender.to_lowercase()
                )),
            )
        })
        .collect();
    format!(
        "{BEGIN}\n-- Managed by `leaf rex install`; `leaf rex uninstall` removes it.\ndofile({}).setup({{\n  bin = {{ {} }},\n  voices = {{\n    {},\n  }},\n}})\n{END}\n",
        lua_string(&plugin_path.display().to_string()),
        lua_string(&bin.display().to_string()),
        voices.join(",\n    "),
    )
}

fn lua_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Puts `block` where the managed block was, or at the end. `None` removes it.
pub fn with_block(content: &str, block: Option<&str>) -> String {
    let replacement = block.unwrap_or("");
    if let Some(start) = content.find(BEGIN)
        && let Some(end_at) = content[start..].find(END)
    {
        let end = start + end_at + END.len();
        let mut after = &content[end..];
        if let Some(stripped) = after.strip_prefix('\n') {
            after = stripped;
        }
        return format!("{}{replacement}{after}", &content[..start]);
    }
    if replacement.is_empty() {
        return content.to_owned();
    }
    if content.trim().is_empty() {
        replacement.to_owned()
    } else {
        format!("{}\n\n{replacement}", content.trim_end_matches('\n'))
    }
}

fn write_if_changed(path: &Path, contents: &str) -> Result<(), String> {
    if path.is_file() && fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn rex(args: &[&str]) -> Result<(), String> {
    let bin = crate::agent::rex_program()?;
    let output = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run rex: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    }
    Err(format!("rex {} failed: {text}", args.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_appended_and_replaced_and_removed() {
        let first = with_block(
            "rex.bind(\"cmd+b\", \"client.sidebar.toggle\")\n",
            Some("BLOCK\n"),
        );
        assert!(first.contains("cmd+b"));
        assert!(first.trim_end().ends_with("BLOCK"));
        let marked = with_block("keep\n", Some(&format!("{BEGIN}\nold\n{END}\n")));
        let replaced = with_block(&marked, Some(&format!("{BEGIN}\nnew\n{END}\n")));
        assert!(replaced.contains("new"));
        assert!(!replaced.contains("old"));
        assert!(replaced.starts_with("keep"));
        let removed = with_block(&replaced, None);
        assert!(removed.starts_with("keep"));
        assert!(!removed.contains("leaf"));
    }
}
