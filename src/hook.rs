//! Wiring Margin into the agent CLIs on this machine: `leaf hook install`
//! finds every agent and sets each up with what it supports, and `leaf hook
//! <agent>` is what an agent's plan-mode hook runs.

use std::fs;
use std::io::{IsTerminal, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::agent;
use crate::margin::{self, Opened};
use crate::review::{Outcome, Verdict};
use crate::skills;

/// Marks the hook entries leaf wrote, so it can update and remove exactly
/// those. It's a shell comment at the end of the command.
const MARK: &str = "# leaf-margin";
/// Seconds an agent waits for the person's review before giving up on it.
const HOOK_TIMEOUT: u64 = 86_400;

/// What leaf can set up for an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Support {
    /// Its plan mode hands the plan to Margin before the agent goes on.
    PlanHook,
    /// The skills, which tell it to run `leaf plan` and `leaf review`.
    Skills,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentCli {
    name: &'static str,
    /// The command, as `leaf agents` finds it.
    binary: &'static str,
    /// Its config folder under the home folder.
    config: &'static str,
    support: Support,
}

/// Every agent leaf knows how to set up. Only Claude Code's plan-mode hook
/// format is checked against its documentation; the others get the skills
/// until theirs is.
const AGENTS: &[AgentCli] = &[
    AgentCli {
        name: "Claude Code",
        binary: "claude",
        config: ".claude",
        support: Support::PlanHook,
    },
    AgentCli {
        name: "Codex",
        binary: "codex",
        config: ".codex",
        support: Support::Skills,
    },
    AgentCli {
        name: "Gemini CLI",
        binary: "gemini",
        config: ".gemini",
        support: Support::Skills,
    },
    AgentCli {
        name: "OpenCode",
        binary: "opencode",
        config: ".config/opencode",
        support: Support::Skills,
    },
    AgentCli {
        name: "Copilot CLI",
        binary: "copilot",
        config: ".copilot",
        support: Support::Skills,
    },
    AgentCli {
        name: "Pi",
        binary: "pi",
        config: ".pi",
        support: Support::Skills,
    },
    AgentCli {
        name: "Cursor Agent",
        binary: "cursor-agent",
        config: ".cursor",
        support: Support::Skills,
    },
    AgentCli {
        name: "Amp",
        binary: "amp",
        config: ".config/amp",
        support: Support::Skills,
    },
    AgentCli {
        name: "Droid",
        binary: "droid",
        config: ".factory",
        support: Support::Skills,
    },
    AgentCli {
        name: "Grok",
        binary: "grok",
        config: ".grok",
        support: Support::Skills,
    },
];

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The agents here: on `PATH` (or where their installers put them), or
/// with a config folder.
fn detect(home: &Path, on_path: &[&str]) -> Vec<AgentCli> {
    AGENTS
        .iter()
        .filter(|cli| on_path.contains(&cli.binary) || home.join(cli.config).is_dir())
        .copied()
        .collect()
}

/// A config file's whole new text, to show and then write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileChange {
    path: PathBuf,
    before: Option<String>,
    after: String,
}

#[derive(Debug, thiserror::Error)]
enum SetupError {
    #[error(
        "{path} isn't valid JSON ({cause}), so leaf left it alone. Fix the file, then run `leaf hook install` again."
    )]
    Unparseable { path: String, cause: String },
    #[error("{path} isn't a JSON object, so leaf left it alone.")]
    NotAnObject { path: String },
}

/// The command an agent's hook runs: this leaf, wherever it's installed.
fn hook_command(exe: &Path, agent: &str) -> String {
    let quoted = format!("'{}'", exe.display().to_string().replace('\'', r"'\''"));
    format!("{quoted} hook {agent} {MARK}")
}

fn is_ours(entry: &Value) -> bool {
    entry["hooks"].as_array().is_some_and(|hooks| {
        hooks
            .iter()
            .any(|hook| hook["command"].as_str().is_some_and(|c| c.contains(MARK)))
    })
}

/// Claude Code's settings with Margin's `ExitPlanMode` hook in them, once.
/// Other settings and hooks are kept as they are, in their order.
fn claude_settings(path: &Path, text: Option<&str>, command: &str) -> Result<String, SetupError> {
    let display = path.display().to_string();
    let mut settings: Value = match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => serde_json::from_str(text).map_err(|e| SetupError::Unparseable {
            path: display.clone(),
            cause: e.to_string(),
        })?,
        None => json!({}),
    };
    let Some(root) = settings.as_object_mut() else {
        return Err(SetupError::NotAnObject { path: display });
    };
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(SetupError::NotAnObject {
            path: format!("{display} (its \"hooks\")"),
        });
    };
    let list = hooks
        .entry("PermissionRequest")
        .or_insert_with(|| json!([]));
    let Some(list) = list.as_array_mut() else {
        return Err(SetupError::NotAnObject {
            path: format!("{display} (its \"hooks.PermissionRequest\")"),
        });
    };
    let ours = json!({
        "matcher": "ExitPlanMode",
        "hooks": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT }],
    });
    match list.iter().position(is_ours) {
        Some(at) => list[at] = ours,
        None => list.push(ours),
    }
    Ok(pretty(&settings))
}

/// Claude Code's settings without Margin's hook; `None` when it had none.
fn claude_settings_without(text: &str) -> Option<String> {
    let mut settings: Value = serde_json::from_str(text).ok()?;
    let hooks = settings.get_mut("hooks")?.as_object_mut()?;
    let list = hooks.get_mut("PermissionRequest")?.as_array_mut()?;
    let before = list.len();
    list.retain(|entry| !is_ours(entry));
    if list.len() == before {
        return None;
    }
    if list.is_empty() {
        hooks.remove("PermissionRequest");
    }
    if hooks.is_empty()
        && let Some(root) = settings.as_object_mut()
    {
        root.remove("hooks");
    }
    Some(pretty(&settings))
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    text.push('\n');
    text
}

/// The lines `after` adds and `before` drops, for the person to read
/// before anything is written.
fn summary(change: &FileChange) -> String {
    let before: Vec<&str> = change.before.as_deref().unwrap_or("").lines().collect();
    let after: Vec<&str> = change.after.lines().collect();
    let mut out = String::new();
    for line in &before {
        if !after.contains(line) {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    for line in &after {
        if !before.contains(line) {
            out.push_str(&format!("  + {line}\n"));
        }
    }
    out
}

/// Writes a change, keeping the file as it first was beside it.
fn apply(change: &FileChange) -> Result<(), String> {
    let path = &change.path;
    if let Some(before) = &change.before {
        let backup = path.with_extension("json.leaf-backup");
        if !backup.exists() {
            fs::write(&backup, before).map_err(|e| format!("{}: {e}", backup.display()))?;
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, &change.after).map_err(|e| format!("{}: {e}", path.display()))
}

fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim(), "y" | "Y" | "yes")
}

const USAGE: &str = "Usage: leaf hook install [--yes] [--dry-run] | uninstall | status

Sets up every agent CLI on this machine for Margin, leaf's review loop.
Claude Code gets a plan-mode hook: when it leaves plan mode, the plan opens
for review beside it, and your verdict decides whether it goes ahead. Every
agent gets the leaf skills, so it can hand you plans (leaf plan) and changes
(leaf review) itself.

install    shows every change, asks once, then makes them (--yes skips asking,
           --dry-run only shows them)
uninstall  removes the hook entries leaf added and the skills it wrote
status     lists each agent and what's set up";

/// `leaf hook`.
pub fn command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]),
        Some("uninstall") => uninstall(),
        Some("status") => status(),
        Some("claude") => run_claude(),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn installed_names() -> Vec<&'static str> {
    agent::installed()
        .into_iter()
        .map(|found| found.name)
        .collect()
}

fn claude_settings_path(home: &Path) -> PathBuf {
    home.join(".claude/settings.json")
}

fn install(args: &[String]) -> ExitCode {
    let mut yes = false;
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--dry-run" => dry_run = true,
            other => {
                eprintln!("leaf: unknown option {other}. Run `leaf hook --help` for usage.");
                return ExitCode::from(2);
            }
        }
    }
    let home = home();
    let found = detect(&home, &installed_names());
    if found.is_empty() {
        println!(
            "No agent CLIs found (looked for {}). Install one, then run `leaf hook install` again.",
            AGENTS
                .iter()
                .map(|a| a.binary)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::SUCCESS;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("leaf"));
    let mut changes = Vec::new();
    let mut problems = Vec::new();
    println!("Agents found:\n");
    for cli in &found {
        let what = match cli.support {
            Support::PlanHook => "plan-mode hook + skills",
            Support::Skills => "skills (run leaf plan / leaf review themselves)",
        };
        println!("  {:<14} {what}", cli.name);
        if cli.support == Support::PlanHook && cli.binary == "claude" {
            let path = claude_settings_path(&home);
            let before = fs::read_to_string(&path).ok();
            match claude_settings(&path, before.as_deref(), &hook_command(&exe, "claude")) {
                Ok(after) if Some(&after) != before.as_ref() => changes.push(FileChange {
                    path,
                    before,
                    after,
                }),
                Ok(_) => {}
                Err(e) => problems.push(e.to_string()),
            }
        }
    }
    let skill_dirs = skills::default_dirs();
    println!(
        "\nSkills (leaf, leaf-plan, leaf-review) go to {}.",
        skill_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(" and ")
    );
    for change in &changes {
        println!("\n{} changes:\n{}", change.path.display(), summary(change));
    }
    for problem in &problems {
        eprintln!("leaf: {problem}");
    }
    if dry_run {
        println!("\nDry run: nothing was written.");
        return ExitCode::SUCCESS;
    }
    if !yes && !confirm("\nMake these changes?") {
        println!(
            "Nothing was changed. Run `leaf hook install --yes` to make them without being asked."
        );
        return ExitCode::SUCCESS;
    }
    let mut failed = false;
    for change in &changes {
        match apply(change) {
            Ok(()) => println!("updated  {}", change.path.display()),
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't write {e}. That file is as it was.");
            }
        }
    }
    for report in skills::install(&skill_dirs) {
        if let Err(e) = report.result {
            failed = true;
            eprintln!("leaf: couldn't write the skill {e}");
        }
    }
    println!("Skills installed. Start a new agent session to pick them up.");
    if failed || !problems.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn uninstall() -> ExitCode {
    let home = home();
    let path = claude_settings_path(&home);
    let mut failed = false;
    if let Some(after) = fs::read_to_string(&path)
        .ok()
        .and_then(|text| claude_settings_without(&text))
    {
        match fs::write(&path, after) {
            Ok(()) => println!("removed the plan-mode hook from {}", path.display()),
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't write {} ({e})", path.display());
            }
        }
    }
    for report in skills::uninstall(&skills::default_dirs()) {
        match report.result {
            Ok(skills::Change::Removed) => println!("removed  {}", report.path.display()),
            Ok(_) => {}
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't remove {e}");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn status() -> ExitCode {
    let home = home();
    let on_path = installed_names();
    let found = detect(&home, &on_path);
    for cli in AGENTS {
        let here = found.contains(cli);
        let state = match (here, cli.support) {
            (false, _) => "not found".to_owned(),
            (true, Support::PlanHook) => {
                let text = fs::read_to_string(claude_settings_path(&home)).unwrap_or_default();
                if text.contains(MARK) {
                    "plan-mode hook installed".to_owned()
                } else {
                    "plan-mode hook not installed: run leaf hook install".to_owned()
                }
            }
            (true, Support::Skills) => "skills only".to_owned(),
        };
        println!("  {:<14} {state}", cli.name);
    }
    let installed = skills::default_dirs()
        .iter()
        .flat_map(|dir| skills::SKILLS.iter().map(move |s| skills::found(dir, *s)))
        .filter(|found| matches!(found, skills::Found::Ours { .. }))
        .count();
    println!(
        "\n  skills         {installed} of {} copies installed",
        skills::SKILLS.len() * 2
    );
    ExitCode::SUCCESS
}

// Running as Claude Code's hook.

/// What Claude Code's `PermissionRequest` hook hands over for `ExitPlanMode`.
#[derive(Debug, Clone, PartialEq)]
struct PlanExit {
    session: String,
    plan: String,
    plan_file: Option<PathBuf>,
    /// Sent back unchanged with an approval, which `ExitPlanMode` needs.
    tool_input: Value,
}

fn parse_claude(text: &str) -> Result<Option<PlanExit>, String> {
    let value: Value =
        serde_json::from_str(text).map_err(|e| format!("the hook input isn't JSON ({e})"))?;
    if value["tool_name"].as_str() != Some("ExitPlanMode") {
        return Ok(None);
    }
    let tool_input = value["tool_input"].clone();
    let plan = tool_input["plan"]
        .as_str()
        .filter(|plan| !plan.trim().is_empty())
        .ok_or("ExitPlanMode came without a plan in tool_input.plan")?
        .to_owned();
    Ok(Some(PlanExit {
        session: value["session_id"].as_str().unwrap_or("session").to_owned(),
        plan,
        plan_file: tool_input["planFilePath"].as_str().map(PathBuf::from),
        tool_input,
    }))
}

/// The hook's answer for an outcome; `None` leaves Claude Code to ask as usual.
fn claude_answer(outcome: &Outcome, tool_input: &Value) -> Option<Value> {
    let decision = match outcome {
        Outcome::Closed => return None,
        Outcome::Sent {
            verdict: Verdict::RequestChanges,
            text,
        } => json!({ "behavior": "deny", "message": text }),
        Outcome::Sent { .. } => json!({ "behavior": "allow", "updatedInput": tool_input }),
    };
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    }))
}

/// The file the person reviews: Claude Code's own plan file when it's
/// there and says the same, else a copy kept with leaf's data.
fn plan_file(exit: &PlanExit) -> Result<PathBuf, String> {
    if let Some(path) = &exit.plan_file
        && fs::read_to_string(path).is_ok_and(|text| text == exit.plan)
    {
        return Ok(path.clone());
    }
    let safe: String = exit
        .session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = crate::progress::default_path()
        .with_file_name("margin")
        .join("plans")
        .join(format!("{safe}.md"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&path, &exit.plan).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// `leaf hook claude`. Whatever goes wrong, it steps aside quietly (no
/// output, exit 0), so Claude Code asks about the plan as it would without
/// leaf, and says why on stderr.
fn run_claude() -> ExitCode {
    let mut input = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("leaf hook: couldn't read the hook input ({e}); Claude Code will ask as usual.");
        return ExitCode::SUCCESS;
    }
    let exit = match parse_claude(&input) {
        Ok(Some(exit)) => exit,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("leaf hook: {e}; Claude Code will ask as usual.");
            return ExitCode::SUCCESS;
        }
    };
    let file = match plan_file(&exit) {
        Ok(file) => file,
        Err(e) => {
            eprintln!(
                "leaf hook: couldn't save the plan for review ({e}); Claude Code will ask as usual."
            );
            return ExitCode::SUCCESS;
        }
    };
    let shown = file.display().to_string();
    let outcome = match margin::beside(
        &["plan".to_owned(), shown.clone()],
        true,
        &format!("leaf plan {shown}"),
    ) {
        Ok(Opened::Waited(outcome)) => outcome,
        Ok(Opened::Returned) => Outcome::Closed,
        Err(e) => {
            eprintln!(
                "leaf hook: {e} Claude Code will ask as usual; the plan is saved at {shown}."
            );
            return ExitCode::SUCCESS;
        }
    };
    // An approval can't carry a message, so notes go to the agent's pane,
    // where they wait as its next message while it starts on the plan.
    if let Outcome::Sent {
        verdict: Verdict::ApproveWithNotes,
        text,
    } = &outcome
        && let Some(pane) = std::env::var("REX_BLOCK").ok().filter(|b| !b.is_empty())
        && let Err(e) = agent::deliver_to(&pane, text)
    {
        eprintln!("leaf hook: the plan is approved, but the notes couldn't be sent ({e}).");
    }
    if let Some(answer) = claude_answer(&outcome, &exit.tool_input) {
        println!("{answer}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-hook-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn agents_are_found_on_path_or_by_their_config_folder() {
        let home = scratch("detect");
        let _ = fs::create_dir_all(home.join(".gemini"));
        let found: Vec<&str> = detect(&home, &["claude", "pi"])
            .iter()
            .map(|cli| cli.binary)
            .collect();
        assert_eq!(found, vec!["claude", "gemini", "pi"]);
        assert!(detect(&scratch("empty"), &[]).is_empty());
    }

    #[test]
    fn the_hook_command_runs_this_leaf_and_carries_the_mark() {
        let command = hook_command(Path::new("/Users/me/it's/leaf"), "claude");
        assert_eq!(
            command,
            r"'/Users/me/it'\''s/leaf' hook claude # leaf-margin"
        );
    }

    #[test]
    fn installing_keeps_other_settings_in_order_and_happens_once() {
        let path = Path::new("/home/me/.claude/settings.json");
        let before = r#"{
  "model": "opus",
  "hooks": {
    "PermissionRequest": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] }
    ],
    "Stop": []
  },
  "alwaysThinkingEnabled": true
}"#;
        let once = claude_settings(path, Some(before), "leaf hook claude # leaf-margin")
            .unwrap_or_default();
        let value: Value = serde_json::from_str(&once).unwrap_or_default();
        let keys: Vec<&String> = value
            .as_object()
            .map(|o| o.keys().collect())
            .unwrap_or_default();
        assert_eq!(keys, ["model", "hooks", "alwaysThinkingEnabled"]);
        let list = &value["hooks"]["PermissionRequest"];
        assert_eq!(list[0]["matcher"], "Bash");
        assert_eq!(list[1]["matcher"], "ExitPlanMode");
        assert_eq!(list[1]["hooks"][0]["timeout"], HOOK_TIMEOUT);
        let twice = claude_settings(path, Some(&once), "leaf hook claude # leaf-margin")
            .unwrap_or_default();
        assert_eq!(once, twice);
        let moved = claude_settings(path, Some(&once), "/new/leaf hook claude # leaf-margin")
            .unwrap_or_default();
        assert_eq!(moved.matches(MARK).count(), 1);
        assert!(moved.contains("/new/leaf"));
    }

    #[test]
    fn a_missing_settings_file_gets_just_the_hook() {
        let path = Path::new("/home/me/.claude/settings.json");
        let made = claude_settings(path, None, "x # leaf-margin").unwrap_or_default();
        let value: Value = serde_json::from_str(&made).unwrap_or_default();
        assert_eq!(
            value["hooks"]["PermissionRequest"][0]["matcher"],
            "ExitPlanMode"
        );
    }

    #[test]
    fn broken_settings_are_left_alone_with_a_reason() {
        let path = Path::new("/home/me/.claude/settings.json");
        assert!(matches!(
            claude_settings(path, Some("{ not json"), "x"),
            Err(SetupError::Unparseable { .. })
        ));
        assert!(matches!(
            claude_settings(path, Some("[1, 2]"), "x"),
            Err(SetupError::NotAnObject { .. })
        ));
        assert!(matches!(
            claude_settings(path, Some(r#"{"hooks": 3}"#), "x"),
            Err(SetupError::NotAnObject { .. })
        ));
    }

    #[test]
    fn uninstalling_takes_out_only_leafs_entry() {
        let path = Path::new("/home/me/.claude/settings.json");
        let alone = claude_settings(path, Some(r#"{"model": "opus"}"#), "x # leaf-margin")
            .unwrap_or_default();
        assert_eq!(
            claude_settings_without(&alone).as_deref(),
            Some("{\n  \"model\": \"opus\"\n}\n")
        );
        let shared = r#"{"hooks": {"PermissionRequest": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "guard"}]},
            {"matcher": "ExitPlanMode", "hooks": [{"type": "command", "command": "x # leaf-margin"}]}
        ]}}"#;
        let kept = claude_settings_without(shared).unwrap_or_default();
        assert!(kept.contains("guard") && !kept.contains(MARK), "{kept}");
        assert_eq!(claude_settings_without(r#"{"model": "opus"}"#), None);
    }

    #[test]
    fn exit_plan_mode_input_is_read_and_other_tools_are_ignored() {
        let input = r##"{"session_id": "abc", "hook_event_name": "PermissionRequest",
            "tool_name": "ExitPlanMode",
            "tool_input": {"plan": "# Plan\n- step", "planFilePath": "/tmp/p.md"}}"##;
        let exit = parse_claude(input).ok().flatten();
        assert_eq!(
            exit.as_ref().map(|e| e.plan.as_str()),
            Some("# Plan\n- step")
        );
        assert_eq!(
            exit.as_ref().and_then(|e| e.plan_file.clone()),
            Some(PathBuf::from("/tmp/p.md"))
        );
        let bash = r#"{"tool_name": "Bash", "tool_input": {"command": "ls"}}"#;
        assert_eq!(parse_claude(bash), Ok(None));
        assert!(parse_claude(r#"{"tool_name": "ExitPlanMode", "tool_input": {}}"#).is_err());
        assert!(parse_claude("nope").is_err());
    }

    #[test]
    fn each_outcome_answers_claude_code_in_its_own_format() {
        let input = json!({ "plan": "p", "planFilePath": "/tmp/p.md" });
        let approve = claude_answer(
            &Outcome::Sent {
                verdict: Verdict::Approve,
                text: "ok".into(),
            },
            &input,
        );
        assert_eq!(
            approve,
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow", "updatedInput": input}}})
            )
        );
        let deny = claude_answer(
            &Outcome::Sent {
                verdict: Verdict::RequestChanges,
                text: "# Review\nfix it".into(),
            },
            &input,
        );
        assert_eq!(
            deny,
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": "# Review\nfix it"}}})
            )
        );
        assert_eq!(claude_answer(&Outcome::Closed, &input), None);
    }
}
