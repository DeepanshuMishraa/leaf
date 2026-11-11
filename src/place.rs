//! Runs leaf in the Rex pane the command palette was called from, rather
//! than in a new one. leaf opens in a layer laid exactly over that pane, so
//! nothing is typed into its shell: quitting leaf closes the layer and the
//! shell is as it was, with no command left on screen or in its history. A
//! leaf document already there is closed first. A pane busy with anything
//! else (an editor, an agent) keeps it, and leaf opens in a split beside it.

use serde_json::{Value, json};

use crate::agent;

const SHELLS: [&str; 10] = [
    "fish", "zsh", "bash", "sh", "dash", "ksh", "nu", "elvish", "xonsh", "pwsh",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// Over the pane, which is back as it was when leaf quits.
    InPane,
    /// The pane was busy, so leaf opened in a split beside it.
    Beside,
}

/// What's in front in a pane, as Rex sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Front {
    Shell,
    Leaf,
    Other,
    Gone,
}

fn front_of(process: &Value) -> Front {
    let name = process["foreground"]["name"]
        .as_str()
        .unwrap_or_default()
        .trim_start_matches('-');
    match name {
        "" => Front::Gone,
        "leaf" => Front::Leaf,
        name if SHELLS.contains(&name) => Front::Shell,
        _ => Front::Other,
    }
}

/// Where `block` sits: its window, and its rect as fractions of that window.
fn locate(session: &Value, block: &str) -> Option<(String, [f64; 4])> {
    fn find(node: &Value, block: &str) -> Option<[f64; 4]> {
        if !node.is_object() {
            return None;
        }
        if node["block_id"].as_str() == Some(block) {
            let rect = node["rect"].as_array()?;
            let at = |i: usize| rect.get(i).and_then(Value::as_f64);
            return Some([at(0)?, at(1)?, at(2)?, at(3)?]);
        }
        let split = &node["split"];
        find(&split["before"], block).or_else(|| find(&split["after"], block))
    }
    session["windows"].as_array()?.iter().find_map(|window| {
        let rect = window["layers"]
            .as_array()?
            .iter()
            .find_map(|layer| find(&layer["layout"], block))?;
        Some((window["window_id"].as_str()?.to_owned(), rect))
    })
}

fn rex_json(args: &[&str]) -> Result<Value, String> {
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    let text = agent::rex_output(&args)?;
    serde_json::from_str(&text).map_err(|e| format!("rex {} printed no JSON: {e}", args.join(" ")))
}

/// Runs this leaf with `args` over `block`, or beside it when it's busy.
pub fn run_in(block: &str, args: &[String], label: &str) -> Result<Placed, String> {
    let session = agent::rex_session_id()
        .ok_or_else(|| "REX_SESSION is unset; this needs a Rex session".to_owned())?;
    let exe = std::env::current_exe().map_err(|e| format!("couldn't find this leaf: {e}"))?;
    let mut command = vec![exe.display().to_string()];
    command.extend(args.iter().cloned());
    let process =
        rex_json(&["block", "call", "-s", &session, "-b", block, "process"]).unwrap_or(Value::Null);
    let front = front_of(&process);
    let cwd = process["foreground"]["cwd"].as_str().map(str::to_owned);
    let layout = rex_json(&["session", "inspect", &session, "--json"])
        .ok()
        .and_then(|view| locate(&view, block));
    match (front, layout) {
        (Front::Shell | Front::Leaf, Some((window, rect))) => {
            if front_of(&process) == Front::Leaf {
                // The document there gives way to the new one.
                agent::rex_write(block, &format!("{}quit\r", "\x1b[24~"))?;
            }
            let mut options = json!({ "command": command });
            if let Some(cwd) = cwd {
                options["cwd"] = json!(cwd);
            }
            let request = json!({
                "window_id": window,
                "bounds": rect,
                "focus": true,
                "layout": { "block": {
                    "flavor": "com.superlogical.terminal.shell",
                    "label": label,
                    "options": options,
                }},
            });
            agent::rex_run(
                &[
                    "api".into(),
                    "call".into(),
                    "-s".into(),
                    session,
                    "session.new_layer".into(),
                    request.to_string(),
                ],
                None,
            )?;
            Ok(Placed::InPane)
        }
        _ => {
            let mut split: Vec<String> = vec!["split".into(), "-s".into(), session];
            if front_of(&process) != Front::Gone {
                split.extend(["-b".into(), block.into()]);
            }
            if let Some(cwd) = cwd {
                split.extend(["-c".into(), cwd]);
            }
            split.extend([
                "--split=right".into(),
                "--label".into(),
                label.into(),
                "--".into(),
            ]);
            split.extend(command);
            agent::rex_run(&split, None)?;
            Ok(Placed::Beside)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_program_in_front_decides_where_leaf_goes() {
        let process = |name: &str| json!({ "foreground": { "pid": 1, "name": name } });
        assert_eq!(front_of(&process("fish")), Front::Shell);
        assert_eq!(front_of(&process("-zsh")), Front::Shell);
        assert_eq!(front_of(&process("leaf")), Front::Leaf);
        assert_eq!(front_of(&process("nvim")), Front::Other);
        assert_eq!(front_of(&Value::Null), Front::Gone);
    }

    #[test]
    fn a_pane_is_found_in_nested_splits_and_floating_layers() {
        let view = json!({ "windows": [{ "window_id": "window:1", "layers": [
            { "layout": { "rect": [0, 0, 1, 1], "split": {
                "before": { "rect": [0, 0, 0.6, 1], "block_id": "block:a" },
                "after": { "rect": [0.6, 0, 0.4, 1], "split": {
                    "before": { "rect": [0.6, 0, 0.4, 0.5], "block_id": "block:b" },
                    "after": { "rect": [0.6, 0.5, 0.4, 0.5], "block_id": "block:c" },
                }},
            }}},
            { "layout": { "rect": [0.2, 0.1, 0.5, 0.5], "block_id": "block:f" } },
        ]}]});
        assert_eq!(
            locate(&view, "block:c"),
            Some(("window:1".to_owned(), [0.6, 0.5, 0.4, 0.5]))
        );
        assert_eq!(
            locate(&view, "block:f").map(|(_, r)| r),
            Some([0.2, 0.1, 0.5, 0.5])
        );
        assert_eq!(locate(&view, "block:z"), None);
    }
}
