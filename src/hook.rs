//! Wiring Margin into the agent CLIs on this machine. `leaf agent setup`
//! finds each one and sets it up the way it supports, and `leaf hook <agent>`
//! is what that agent's hook runs when it has a plan to show.
//!
//! How each agent hands over its plan:
//!
//! - Claude Code: a `PermissionRequest` hook on `ExitPlanMode`.
//! - Grok Build: a `PreToolUse` hook on `exit_plan_mode`.
//! - Codex: a `Stop` hook that finds `<proposed_plan>` in plan mode's answer.
//! - Antigravity: a `Stop` hook that finds a new `implementation_plan.md`.
//! - Cursor: a `preToolUse` hook on its plan tool.
//! - Pi: an extension with `leaf_review_plan` and `leaf_review_changes` tools.
//! - OpenCode: the skills, since it has no plan hook to attach to.

use std::fs;
use std::io::{IsTerminal, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::agent;
use crate::margin::{self, Opened, Reply};
use crate::plugin::fnv1a;
use crate::review::{Outcome, Verdict};
use crate::skills;

/// Marks what leaf wrote into another tool's config, so it can update and
/// remove exactly that. In a command it's a shell comment at the end.
const MARK: &str = "leaf-margin";
/// Seconds an agent waits for the person's review before giving up on it.
const HOOK_TIMEOUT: u64 = 86_400;
const PI_EXTENSION: &str = include_str!("../integrations/pi/leaf-margin.ts");

/// The agents leaf sets up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Agent {
    Claude,
    Codex,
    Antigravity,
    Grok,
    Cursor,
    Pi,
    OpenCode,
}

impl Agent {
    const ALL: [Agent; 7] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Antigravity,
        Agent::Grok,
        Agent::Cursor,
        Agent::Pi,
        Agent::OpenCode,
    ];

    fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Antigravity => "Antigravity",
            Agent::Grok => "Grok Build",
            Agent::Cursor => "Cursor",
            Agent::Pi => "Pi",
            Agent::OpenCode => "OpenCode",
        }
    }

    /// The word `leaf hook <word>` takes for it.
    fn word(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Antigravity => "antigravity",
            Agent::Grok => "grok",
            Agent::Cursor => "cursor",
            Agent::Pi => "pi",
            Agent::OpenCode => "opencode",
        }
    }

    fn binaries(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["claude"],
            Agent::Codex => &["codex"],
            Agent::Antigravity => &["agy", "antigravity"],
            Agent::Grok => &["grok"],
            Agent::Cursor => &["cursor-agent"],
            Agent::Pi => &["pi"],
            Agent::OpenCode => &["opencode"],
        }
    }

    /// Its config folder, under the home folder.
    fn config(self) -> &'static str {
        match self {
            Agent::Claude => ".claude",
            Agent::Codex => ".codex",
            Agent::Antigravity => ".gemini/antigravity-cli",
            Agent::Grok => ".grok",
            Agent::Cursor => ".cursor",
            Agent::Pi => ".pi/agent",
            Agent::OpenCode => ".config/opencode",
        }
    }

    /// Where it reads skills; several agents share `~/.agents/skills`.
    fn skills_dir(self, home: &Path) -> PathBuf {
        home.join(match self {
            Agent::Claude => ".claude/skills",
            Agent::Antigravity => ".gemini/config/skills",
            Agent::Cursor => ".cursor/skills",
            Agent::Codex | Agent::Grok | Agent::Pi | Agent::OpenCode => ".agents/skills",
        })
    }

    /// What the person sees about it in the install list.
    fn setup(self) -> &'static str {
        match self {
            Agent::Claude | Agent::Grok | Agent::Cursor => "plans open in leaf before it builds",
            Agent::Codex => "plans open in leaf before it builds (trust it once in /hooks)",
            Agent::Antigravity => "implementation plans open in leaf",
            Agent::Pi => "leaf_review_plan and leaf_review_changes tools",
            Agent::OpenCode => "skills: it runs leaf plan and leaf review itself",
        }
    }

    /// The file leaf writes its hook into, when it has one.
    fn hook_file(self, home: &Path) -> Option<PathBuf> {
        Some(home.join(match self {
            Agent::Claude => ".claude/settings.json",
            Agent::Codex => ".codex/hooks.json",
            Agent::Antigravity => ".gemini/config/hooks.json",
            Agent::Grok => ".grok/hooks/leaf-margin.json",
            Agent::Cursor => ".cursor/hooks.json",
            Agent::Pi => ".pi/agent/extensions/leaf-margin.ts",
            Agent::OpenCode => return None,
        }))
    }

    fn parse(word: &str) -> Option<Agent> {
        Agent::ALL
            .into_iter()
            .find(|agent| agent.word() == word || (word == "agy" && *agent == Agent::Antigravity))
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Folders a command may be installed in, beyond `PATH`.
fn search_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for extra in [
        ".local/bin",
        ".bun/bin",
        ".npm-global/bin",
        ".cargo/bin",
        ".opencode/bin",
        ".grok/bin",
    ] {
        dirs.push(home.join(extra));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs
}

/// The agents here: installed as a command, or with a config folder.
fn detect(home: &Path, dirs: &[PathBuf]) -> Vec<Agent> {
    Agent::ALL
        .into_iter()
        .filter(|agent| {
            home.join(agent.config()).is_dir()
                || agent
                    .binaries()
                    .iter()
                    .any(|bin| dirs.iter().any(|dir| dir.join(bin).is_file()))
        })
        .collect()
}

/// A file's whole new text, to show and then write.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileChange {
    path: PathBuf,
    before: Option<String>,
    after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum SetupError {
    #[error(
        "{path} isn't valid JSON ({cause}), so leaf left it alone. Fix the file, then run `leaf agent setup` again."
    )]
    Unparseable { path: String, cause: String },
    #[error("{path} has an unexpected shape ({what}), so leaf left it alone.")]
    Shape { path: String, what: String },
}

/// The command an agent's hook runs: this leaf, wherever it's installed.
fn hook_command(exe: &Path, agent: Agent) -> String {
    format!("{} hook {} # {MARK}", shell_quote(exe), agent.word())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

fn read_json(path: &Path, text: Option<&str>) -> Result<Value, SetupError> {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => serde_json::from_str(text).map_err(|e| SetupError::Unparseable {
            path: path.display().to_string(),
            cause: e.to_string(),
        }),
        None => Ok(json!({})),
    }
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    text.push('\n');
    text
}

fn object<'a>(
    value: &'a mut Value,
    path: &Path,
    what: &str,
) -> Result<&'a mut Map<String, Value>, SetupError> {
    value.as_object_mut().ok_or_else(|| SetupError::Shape {
        path: path.display().to_string(),
        what: format!("{what} isn't an object"),
    })
}

fn has_mark(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains(MARK),
        Value::Array(items) => items.iter().any(has_mark),
        Value::Object(map) => map.values().any(has_mark),
        _ => false,
    }
}

/// `root.hooks.<event>` with `entry` in it once, in place of leaf's earlier one.
fn upsert_event(
    path: &Path,
    text: Option<&str>,
    event: &str,
    entry: Value,
    top: &[(&str, Value)],
) -> Result<String, SetupError> {
    let mut settings = read_json(path, text)?;
    let root = object(&mut settings, path, "the file")?;
    for (key, value) in top {
        root.entry(*key).or_insert_with(|| value.clone());
    }
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    let hooks = object(hooks, path, "\"hooks\"")?;
    let list = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(list) = list.as_array_mut() else {
        return Err(SetupError::Shape {
            path: path.display().to_string(),
            what: format!("\"hooks.{event}\" isn't a list"),
        });
    };
    match list.iter_mut().find(|item| has_mark(item)) {
        Some(found) => refresh(found, entry),
        None => list.push(entry),
    }
    Ok(pretty(&settings))
}

/// Updates leaf's own parts of `existing` from `fresh`, leaving anything else
/// in the entry (another command in the same hook list, say) as it was.
fn refresh(existing: &mut Value, fresh: Value) {
    match (existing, fresh) {
        (Value::Object(old), Value::Object(new)) => {
            for (key, value) in new {
                match old.get_mut(&key) {
                    Some(slot) => refresh(slot, value),
                    None => {
                        old.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for value in new {
                match old.iter_mut().find(|item| has_mark(item)) {
                    Some(slot) => refresh(slot, value),
                    None => old.push(value),
                }
            }
        }
        (slot, fresh) => *slot = fresh,
    }
}

/// The new text of `agent`'s hook file with leaf in it, or `None` when it
/// gets no hook.
fn installed_text(
    agent: Agent,
    path: &Path,
    before: Option<&str>,
    exe: &Path,
) -> Result<Option<String>, SetupError> {
    let command = hook_command(exe, agent);
    let handler = json!({ "type": "command", "command": command, "timeout": HOOK_TIMEOUT });
    let text = match agent {
        Agent::Claude => upsert_event(
            path,
            before,
            "PermissionRequest",
            json!({ "matcher": "ExitPlanMode", "hooks": [handler] }),
            &[],
        )?,
        Agent::Codex => upsert_event(path, before, "Stop", json!({ "hooks": [handler] }), &[])?,
        Agent::Cursor => upsert_event(
            path,
            before,
            "preToolUse",
            json!({ "command": command, "timeout": HOOK_TIMEOUT }),
            &[("version", json!(1))],
        )?,
        Agent::Grok => pretty(&json!({
            "hooks": {
                "PreToolUse": [{ "matcher": "exit_plan_mode", "hooks": [handler] }]
            }
        })),
        Agent::Antigravity => {
            let mut settings = read_json(path, before)?;
            let root = object(&mut settings, path, "the file")?;
            root.insert(
                MARK.to_owned(),
                json!({ "Stop": [{ "type": "command", "command": command, "timeout": HOOK_TIMEOUT }] }),
            );
            pretty(&settings)
        }
        Agent::Pi => {
            // The path sits inside a TypeScript string literal, so escape it
            // as a JSON string's contents rather than its quoted form.
            let quoted = Value::String(exe.display().to_string()).to_string();
            let escaped = quoted
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(&quoted);
            PI_EXTENSION.replace("__LEAF__", escaped)
        }
        Agent::OpenCode => return Ok(None),
    };
    Ok(Some(text))
}

/// The lines `after` adds and `before` drops, for the person to read
/// before anything is written.
fn summary(change: &FileChange) -> String {
    let before: Vec<&str> = change.before.as_deref().unwrap_or("").lines().collect();
    let after: Vec<&str> = change.after.lines().collect();
    let mut out = String::new();
    for line in before.iter().filter(|line| !after.contains(line)) {
        out.push_str(&format!("    - {line}\n"));
    }
    for line in after.iter().filter(|line| !before.contains(line)) {
        out.push_str(&format!("    + {line}\n"));
    }
    out
}

/// Writes a change, keeping a JSON file as it first was beside it. The write
/// goes to a temporary file first so a failure can't truncate the original.
fn apply(change: &FileChange) -> Result<(), String> {
    let path = &change.path;
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let (Some(before), Some(extension)) = (&change.before, path.extension())
        && extension == "json"
    {
        let backup = path.with_extension("json.leaf-backup");
        if !backup.exists() {
            fs::write(&backup, before).map_err(fail)?;
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(fail)?;
    }
    atomic_write(path, &change.after).map_err(fail)
}

/// Replaces `path` through a temporary file, so a failed write leaves the old
/// text whole. A symlinked config is written at its target, keeping the link,
/// and a read-only one is refused as a plain write would be.
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let path = target.as_path();
    if fs::metadata(path).is_ok_and(|meta| meta.permissions().readonly()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the file is read-only",
        ));
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "leaf".to_owned());
    let temp = path.with_file_name(format!(".{name}.leaf-{}.tmp", std::process::id()));
    fs::write(&temp, text)?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&temp, meta.permissions());
    }
    fs::rename(&temp, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    print!("{question} [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim(), "" | "y" | "Y" | "yes")
}

fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

const USAGE: &str = "Usage: leaf agent setup [--yes] [--dry-run] | skill <name>

Sets up the coding agents on this machine to hand you their plans and changes
for review in leaf: Claude Code, Codex, Antigravity, Grok Build, Cursor, Pi
and OpenCode. Each gets the leaf skills, and a hook where it has one.

setup      shows what will change, asks once, then does it
           (--yes doesn't ask, --dry-run only shows)
skill      prints one skill to stdout, for agents that take a file:
           leaf, leaf-plan, leaf-review or leaf-config";

const HOOK_USAGE: &str = "Usage: leaf hook <agent>

Called by an agent's hook, not by hand: it opens leaf for the plan or the
changes the agent is handing over, and answers in that agent's own format.
Agents: claude, codex, antigravity (agy), grok, cursor, pi, opencode.
Set the hooks up with `leaf agent setup`.";

/// `leaf agent`.
pub fn agent_command(args: &[String]) -> ExitCode {
    let Some((verb, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match verb.as_str() {
        "setup" => setup(rest),
        "skill" => skill(rest),
        "-h" | "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// `leaf agent skill <name>`, which prints one skill for agents that take a file.
fn skill(args: &[String]) -> ExitCode {
    match args {
        [name] => match skills::SKILLS.iter().find(|skill| skill.name == name) {
            Some(skill) => {
                print!("{}", skill.text());
                ExitCode::SUCCESS
            }
            None => {
                let names: Vec<&str> = skills::SKILLS.iter().map(|skill| skill.name).collect();
                eprintln!(
                    "leaf: there's no skill called {name}. The skills are {}.",
                    names.join(", ")
                );
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!(
                "leaf: name the skill to print: leaf, leaf-plan, leaf-review or leaf-config."
            );
            ExitCode::from(2)
        }
    }
}

/// `leaf hook`, the entry point an agent's hook runs.
pub fn command(args: &[String]) -> ExitCode {
    match args {
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{HOOK_USAGE}");
            ExitCode::SUCCESS
        }
        [word] => match Agent::parse(word) {
            Some(agent) => run(agent),
            None => {
                eprintln!("{HOOK_USAGE}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{HOOK_USAGE}");
            ExitCode::from(2)
        }
    }
}

/// What installing would change, and anything that can't be set up.
fn plan_install(home: &Path, agents: &[Agent], exe: &Path) -> (Vec<FileChange>, Vec<String>) {
    let mut changes = Vec::new();
    let mut problems = Vec::new();
    for agent in agents {
        let Some(path) = agent.hook_file(home) else {
            continue;
        };
        let before = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                problems.push(format!(
                    "{}: {} couldn't be read ({e}), so leaf left it alone.",
                    agent.name(),
                    path.display()
                ));
                continue;
            }
        };
        match installed_text(*agent, &path, before.as_deref(), exe) {
            Ok(Some(after)) if Some(&after) != before.as_ref() => changes.push(FileChange {
                path,
                before,
                after,
            }),
            Ok(_) => {}
            Err(e) => problems.push(format!("{}: {e}", agent.name())),
        }
    }
    (changes, problems)
}

/// The folders setup writes skills into: the shared ones always, so they're
/// ready for an agent installed later, plus each detected agent's own.
fn skill_dirs(home: &Path, agents: &[Agent]) -> Vec<PathBuf> {
    let mut dirs = skills::default_dirs(home);
    for agent in agents {
        let dir = agent.skills_dir(home);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// `1 file`, `3 files`.
fn files(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

/// What setup would change, one line each, then the files it leaves alone.
fn show_plan(home: &Path, changes: &[FileChange], planned: &[skills::Planned], diff: bool) {
    for change in changes {
        let verb = if change.before.is_some() {
            "update"
        } else {
            "add"
        };
        println!("  {verb:<10} {} (the plan hook)", tilde(&change.path, home));
        if diff {
            print!("{}", summary(change));
        }
    }
    let mut current = 0;
    let mut kept = Vec::new();
    for item in planned {
        if item.change.writes() {
            println!("  {:<10} {}", item.change.label(), tilde(&item.path, home));
        } else if item.change == skills::Change::AlreadyCurrent {
            current += 1;
        } else {
            kept.push(item);
        }
    }
    if current > 0 {
        let plural = if current == 1 { "" } else { "s" };
        println!("  {current} skill{plural} already current");
    }
    if !kept.is_empty() {
        println!(
            "  {} left alone, edited by hand or not leaf's:",
            files(kept.len())
        );
        for item in kept {
            println!("    {}", tilde(&item.path, home));
        }
    }
}

fn setup(args: &[String]) -> ExitCode {
    let mut yes = false;
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--yes" | "-y" => yes = true,
            "--dry-run" | "-n" => dry_run = true,
            other => {
                eprintln!("leaf: unknown option {other}. Run `leaf agent --help` for usage.");
                return ExitCode::from(2);
            }
        }
    }
    let home = home();
    let agents = detect(&home, &search_dirs(&home));
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("leaf"));
    let (changes, problems) = plan_install(&home, &agents, &exe);
    let dirs = skill_dirs(&home, &agents);
    let planned = skills::plan(&dirs);
    let writing = changes.len() + planned.iter().filter(|item| item.change.writes()).count();
    println!();
    if agents.is_empty() {
        println!(
            "No coding agents found here yet. leaf works with Claude Code, Codex, Antigravity, Grok Build, Cursor, Pi and OpenCode; the skills below are ready for when you install one."
        );
    }
    for agent in &agents {
        println!("  {:<13} {}", agent.name(), agent.setup());
    }
    println!();
    show_plan(&home, &changes, &planned, dry_run);
    for problem in &problems {
        eprintln!("\n  ! {problem}");
    }
    if dry_run {
        println!("\nNothing written (dry run).");
        return ExitCode::SUCCESS;
    }
    if writing == 0 {
        println!("\nEverything is already set up.");
        return if problems.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    if !yes && !confirm("\nSet these up?") {
        println!("Nothing changed. `leaf agent setup --yes` sets them up without asking.");
        return ExitCode::SUCCESS;
    }
    let mut written = 0;
    let mut failed = !problems.is_empty();
    for change in &changes {
        match apply(change) {
            Ok(()) => {
                written += 1;
                let verb = if change.before.is_some() {
                    "updated"
                } else {
                    "added"
                };
                println!(
                    "  {verb:<10} {} (the plan hook)",
                    tilde(&change.path, &home)
                );
            }
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't write {e}. That file is as it was.");
            }
        }
    }
    for report in skills::install(&dirs) {
        match report.result {
            Ok(change) if change.writes() => {
                written += 1;
                let verb = match change {
                    skills::Change::Written => "added",
                    skills::Change::Updated => "updated",
                    _ => unreachable!("only writes have a success message"),
                };
                println!("  {verb:<10} {}", tilde(&report.path, &home));
            }
            Ok(_) => {}
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't write the skill {e}. That skill is as it was.");
            }
        }
    }
    println!(
        "\nSetup finished: {} written. Start a new agent session to pick it up.",
        files(written)
    );
    if agents.contains(&Agent::Codex) {
        println!("Codex runs a new hook only once you trust it: open /hooks in Codex.");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

// Running as an agent's hook. Whatever goes wrong, a hook steps aside
// quietly (no output, exit 0) so the agent carries on as it would without
// leaf, and says why on stderr.

/// What a hook found to review.
#[derive(Debug, Clone, PartialEq)]
struct Handoff {
    plan: String,
    /// The agent's own file holding the plan, when it keeps one.
    file: Option<PathBuf>,
    /// Names this conversation, for the file leaf keeps the plan in.
    session: String,
    /// The agent's tool input, which some answers send back.
    input: Value,
}

fn str_at<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
}

/// The text between `<proposed_plan>` tags, which Codex's plan mode ends with.
fn proposed_plan(message: &str) -> Option<String> {
    let start = message.rfind("<proposed_plan>")? + "<proposed_plan>".len();
    let end = start + message[start..].find("</proposed_plan>")?;
    let plan = message[start..end].trim();
    (!plan.is_empty()).then(|| plan.to_owned())
}

/// The plan an agent's hook input carries, or `None` when this call isn't
/// a plan being handed over.
fn handoff(agent: Agent, input: &Value, home: &Path) -> Option<Handoff> {
    let session = str_at(
        input,
        &[
            "session_id",
            "sessionId",
            "conversation_id",
            "conversationId",
        ],
    )
    .unwrap_or("session")
    .to_owned();
    match agent {
        Agent::Claude => {
            (str_at(input, &["tool_name"]) == Some("ExitPlanMode")).then_some(())?;
            let tool = input.get("tool_input")?;
            Some(Handoff {
                plan: str_at(tool, &["plan"])?.to_owned(),
                file: str_at(tool, &["planFilePath"]).map(PathBuf::from),
                session,
                input: tool.clone(),
            })
        }
        Agent::Grok => {
            let name = str_at(input, &["toolName", "tool_name"])?;
            matches!(name, "exit_plan_mode" | "ExitPlanMode").then_some(())?;
            let tool = input
                .get("toolInput")
                .or_else(|| input.get("tool_input"))
                .cloned()
                .unwrap_or(Value::Null);
            let file = grok_plan_file(home, &session);
            let plan = str_at(&tool, &["plan"])
                .map(str::to_owned)
                .or_else(|| file.as_ref().and_then(|f| fs::read_to_string(f).ok()))
                .filter(|plan| !plan.trim().is_empty())?;
            Some(Handoff {
                plan,
                file,
                session,
                input: tool,
            })
        }
        Agent::Codex => {
            let message = str_at(input, &["last_assistant_message"])?;
            Some(Handoff {
                plan: proposed_plan(message)?,
                file: None,
                session,
                input: Value::Null,
            })
        }
        Agent::Antigravity => {
            let dir = PathBuf::from(str_at(input, &["artifactDirectoryPath"])?);
            let file = dir.join("implementation_plan.md");
            let plan = fs::read_to_string(&file).ok()?;
            (!plan.trim().is_empty()).then_some(())?;
            Some(Handoff {
                plan,
                file: Some(file),
                session,
                input: Value::Null,
            })
        }
        Agent::Cursor => {
            let name = str_at(input, &["tool_name"])?;
            name.to_ascii_lowercase().contains("plan").then_some(())?;
            let tool = input.get("tool_input")?;
            Some(Handoff {
                plan: str_at(tool, &["plan"])?.to_owned(),
                file: None,
                session,
                input: tool.clone(),
            })
        }
        Agent::Pi | Agent::OpenCode => None,
    }
}

/// Grok keeps the plan in `~/.grok/sessions/<folder>/<session>/plan.md`.
fn grok_plan_file(home: &Path, session: &str) -> Option<PathBuf> {
    fs::read_dir(home.join(".grok/sessions"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(session).join("plan.md"))
        .find(|path| path.is_file())
}

fn safe_name(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

/// The file the person reviews: the agent's own when it holds the plan as
/// handed over, else a copy kept with leaf's data, one per conversation so
/// later rounds compare against it.
fn review_file(agent: Agent, handoff: &Handoff, data: &Path) -> Result<PathBuf, String> {
    if let Some(path) = &handoff.file
        && fs::read_to_string(path).is_ok_and(|text| text == handoff.plan)
    {
        return Ok(path.clone());
    }
    let path = data.join("plans").join(format!(
        "{}-{}.md",
        agent.word(),
        safe_name(&handoff.session)
    ));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(&path, &handoff.plan).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// The file that records the last plan reviewed in this conversation.
fn seen_path(agent: Agent, handoff: &Handoff, data: &Path) -> PathBuf {
    data.join("seen")
        .join(format!("{}-{}", agent.word(), safe_name(&handoff.session)))
}

/// Whether this exact plan was already reviewed in this conversation, for
/// the agents whose hook fires at every turn's end.
fn seen_before(agent: Agent, handoff: &Handoff, data: &Path) -> bool {
    let stamp = format!("{:016x}", fnv1a(&handoff.plan));
    fs::read_to_string(seen_path(agent, handoff, data)).is_ok_and(|seen| seen.trim() == stamp)
}

/// Records the plan as reviewed, so the same one isn't shown again.
fn remember(agent: Agent, handoff: &Handoff, data: &Path) {
    let path = seen_path(agent, handoff, data);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, format!("{:016x}", fnv1a(&handoff.plan)));
}

/// What the agent is told for an outcome; `None` says nothing, so it goes
/// on as it would have without leaf.
fn answer(agent: Agent, outcome: &Outcome, input: &Value) -> Option<Value> {
    let Outcome::Sent { verdict, text } = outcome else {
        return None;
    };
    let changes = *verdict == Verdict::RequestChanges;
    match agent {
        Agent::Claude => Some(json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": if changes {
                    json!({ "behavior": "deny", "message": text })
                } else {
                    json!({ "behavior": "allow", "updatedInput": input })
                },
            }
        })),
        Agent::Grok => Some(if changes {
            json!({ "decision": "deny", "reason": text })
        } else {
            json!({ "decision": "allow" })
        }),
        Agent::Cursor => Some(if changes {
            json!({ "permission": "deny", "agent_message": text, "user_message": "Changes requested in leaf." })
        } else {
            json!({ "permission": "allow" })
        }),
        // Still planning, so notes go back into the plan too.
        Agent::Codex => {
            (*verdict != Verdict::Approve).then(|| json!({ "decision": "block", "reason": text }))
        }
        Agent::Antigravity => (*verdict != Verdict::Approve)
            .then(|| json!({ "decision": "continue", "reason": text })),
        Agent::Pi | Agent::OpenCode => None,
    }
}

/// Agents whose answer can't carry an approval's notes get them as their
/// next message instead, typed into their pane.
fn notes_by_message(agent: Agent) -> bool {
    matches!(agent, Agent::Claude | Agent::Grok | Agent::Cursor)
}

fn run(agent: Agent) -> ExitCode {
    let name = agent.name();
    let mut text = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut text) {
        eprintln!("leaf: couldn't read {name}'s hook input ({e}); it carries on as usual.");
        return ExitCode::SUCCESS;
    }
    let Ok(input) = serde_json::from_str::<Value>(&text) else {
        eprintln!("leaf: {name}'s hook input isn't JSON; it carries on as usual.");
        return ExitCode::SUCCESS;
    };
    let Some(handoff) = handoff(agent, &input, &home()) else {
        return ExitCode::SUCCESS;
    };
    let data = margin::data_dir();
    let watches = matches!(agent, Agent::Codex | Agent::Antigravity);
    if watches && seen_before(agent, &handoff, &data) {
        return ExitCode::SUCCESS;
    }
    let file = match review_file(agent, &handoff, &data) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("leaf: couldn't keep the plan for review ({e}); {name} carries on as usual.");
            return ExitCode::SUCCESS;
        }
    };
    let shown = file.display().to_string();
    let title = format!("{name}'s plan");
    let outcome = match margin::beside(
        &[
            "plan".to_owned(),
            "--title".to_owned(),
            title,
            shown.clone(),
        ],
        true,
        &format!("leaf plan {shown}"),
        &Reply::Print,
    ) {
        Ok(Opened::Waited(outcome)) => outcome,
        Ok(Opened::Returned) => Outcome::Closed,
        Err(e) => {
            eprintln!("leaf: {e} {name} carries on as usual; the plan is at {shown}.");
            return ExitCode::SUCCESS;
        }
    };
    // Only a review that went through counts as seen, so closing it or
    // failing to open it leaves the plan open to a later try.
    if watches && matches!(outcome, Outcome::Sent { .. }) {
        remember(agent, &handoff, &data);
    }
    if let Outcome::Sent {
        verdict: Verdict::ApproveWithNotes,
        text,
    } = &outcome
        && notes_by_message(agent)
        && let Some(pane) = crate::host::Host::detect().and_then(|host| host.own_pane())
        && let Err(e) = agent::deliver_to_pane(&pane, text)
    {
        eprintln!("leaf: the plan is approved, but the notes couldn't be sent ({e}).");
    }
    if let Some(answer) = answer(agent, &outcome, &handoff.input) {
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

    const EXE: &str = "/opt/leaf/bin/leaf";

    fn install_text(agent: Agent, before: Option<&str>) -> String {
        let path = PathBuf::from("/home/me/x.json");
        installed_text(agent, &path, before, Path::new(EXE))
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn sent(verdict: Verdict) -> Outcome {
        Outcome::Sent {
            verdict,
            text: "# Review".into(),
        }
    }

    #[test]
    fn agents_are_found_by_command_or_config_folder() {
        let home = scratch("detect");
        let bin = home.join("bin");
        let _ = fs::create_dir_all(home.join(".gemini/antigravity-cli"));
        let _ = fs::create_dir_all(&bin);
        let _ = fs::write(bin.join("codex"), "");
        let _ = fs::write(bin.join("cursor-agent"), "");
        assert_eq!(
            detect(&home, &[bin]),
            vec![Agent::Codex, Agent::Antigravity, Agent::Cursor]
        );
        assert!(detect(&scratch("none"), &[]).is_empty());
    }

    #[test]
    fn setup_writes_the_shared_skill_folders_even_with_no_agents() {
        let home = PathBuf::from("/Users/me");
        assert_eq!(
            skill_dirs(&home, &[]),
            vec![home.join(".claude/skills"), home.join(".agents/skills")]
        );
        let dirs = skill_dirs(&home, &Agent::ALL);
        for agent in Agent::ALL {
            assert!(dirs.contains(&agent.skills_dir(&home)), "{}", agent.name());
        }
    }

    #[test]
    fn the_hook_command_runs_this_leaf_and_carries_the_mark() {
        assert_eq!(
            hook_command(Path::new("/Users/me/it's/leaf"), Agent::Grok),
            r"'/Users/me/it'\''s/leaf' hook grok # leaf-margin"
        );
        for agent in Agent::ALL {
            assert_eq!(Agent::parse(agent.word()), Some(agent));
        }
        assert_eq!(Agent::parse("agy"), Some(Agent::Antigravity));
    }

    #[test]
    fn claude_keeps_other_settings_in_order_and_gets_one_hook() {
        let before = r#"{
  "model": "opus",
  "hooks": {
    "PermissionRequest": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] }
    ]
  },
  "alwaysThinkingEnabled": true
}"#;
        let once = install_text(Agent::Claude, Some(before));
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
        assert_eq!(install_text(Agent::Claude, Some(&once)), once);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_written_through_its_link() {
        let dir = std::env::temp_dir().join(format!("leaf-hook-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.json");
        let link = dir.join("settings.json");
        fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        atomic_write(&link, "{\"a\":1}").unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "{\"a\":1}");

        let locked = dir.join("locked.json");
        fs::write(&locked, "{}").unwrap();
        let mut permissions = fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&locked, permissions).unwrap();
        assert!(atomic_write(&locked, "{\"a\":1}").is_err());
        assert_eq!(fs::read_to_string(&locked).unwrap(), "{}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn another_handler_in_leafs_hook_entry_is_kept() {
        let command = format!("'{EXE}' hook claude # {MARK}");
        let before = format!(
            r#"{{"hooks":{{"PermissionRequest":[{{"matcher":"ExitPlanMode","hooks":[{{"type":"command","command":"{command}"}},{{"type":"command","command":"audit"}}]}}]}}}}"#
        );
        let once = install_text(Agent::Claude, Some(&before));
        let value: Value = serde_json::from_str(&once).unwrap_or_default();
        let hooks = value["hooks"]["PermissionRequest"][0]["hooks"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(hooks.len(), 2, "{once}");
        assert!(
            hooks.iter().any(|hook| hook["command"] == "audit"),
            "{once}"
        );
        assert!(
            hooks.iter().any(|hook| hook["command"] == command),
            "{once}"
        );
        assert_eq!(install_text(Agent::Claude, Some(&once)), once);
    }

    #[test]
    fn codex_gets_a_stop_hook_beside_its_others() {
        let before =
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "rex"}]}]}}"#;
        let text = install_text(Agent::Codex, Some(before));
        let value: Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(
            value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "rex"
        );
        assert!(
            value["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.ends_with("hook codex # leaf-margin"))
        );
        assert_eq!(install_text(Agent::Codex, Some(&text)), text);
    }

    #[test]
    fn cursor_gets_a_flat_pre_tool_use_entry_and_a_version() {
        let text = install_text(Agent::Cursor, None);
        let value: Value = serde_json::from_str(&text).unwrap_or_default();
        assert_eq!(value["version"], 1);
        assert!(
            value["hooks"]["preToolUse"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains("hook cursor"))
        );
        let kept = install_text(
            Agent::Cursor,
            Some(r#"{"version": 1, "hooks": {"preToolUse": [{"command": "git-ai"}]}}"#),
        );
        let value: Value = serde_json::from_str(&kept).unwrap_or_default();
        assert_eq!(value["hooks"]["preToolUse"][0]["command"], "git-ai");
        assert_eq!(
            value["hooks"]["preToolUse"].as_array().map(Vec::len),
            Some(2)
        );
    }

    #[test]
    fn antigravity_gets_a_named_hook_and_grok_and_pi_their_own_files() {
        let text = install_text(Agent::Antigravity, Some(r#"{"safety": {"Stop": []}}"#));
        let value: Value = serde_json::from_str(&text).unwrap_or_default();
        assert!(value.get("safety").is_some());
        assert_eq!(value[MARK]["Stop"][0]["timeout"], HOOK_TIMEOUT);
        assert_eq!(install_text(Agent::Antigravity, Some(&text)), text);
        let grok = install_text(Agent::Grok, None);
        assert!(grok.contains("\"exit_plan_mode\""));
        let pi = install_text(Agent::Pi, None);
        assert!(pi.contains(&format!("const LEAF = \"{EXE}\"")));
        assert!(pi.contains("Delete this file to remove it"));
    }

    #[test]
    fn opencode_gets_no_hook_file() {
        assert_eq!(Agent::OpenCode.hook_file(Path::new("/h")), None);
    }

    #[test]
    fn broken_config_is_left_alone_with_a_reason() {
        let path = Path::new("/home/me/.claude/settings.json");
        assert!(matches!(
            installed_text(Agent::Claude, path, Some("{ nope"), Path::new(EXE)),
            Err(SetupError::Unparseable { .. })
        ));
        assert!(matches!(
            installed_text(
                Agent::Codex,
                path,
                Some(r#"{"hooks": {"Stop": 3}}"#),
                Path::new(EXE)
            ),
            Err(SetupError::Shape { .. })
        ));
    }

    #[test]
    fn each_agents_hook_input_yields_its_plan() {
        let home = scratch("handoff");
        let claude = json!({"session_id": "s", "tool_name": "ExitPlanMode",
            "tool_input": {"plan": "# P", "planFilePath": "/tmp/p.md"}});
        let found = handoff(Agent::Claude, &claude, &home);
        assert_eq!(found.as_ref().map(|h| h.plan.as_str()), Some("# P"));
        assert_eq!(found.and_then(|h| h.file), Some(PathBuf::from("/tmp/p.md")));
        assert_eq!(
            handoff(
                Agent::Claude,
                &json!({"tool_name": "Bash", "tool_input": {}}),
                &home
            ),
            None
        );

        let codex = json!({"session_id": "c1", "permission_mode": "plan",
            "last_assistant_message": "Here it is.\n<proposed_plan>\n# Plan\n- a\n</proposed_plan>"});
        assert_eq!(
            handoff(Agent::Codex, &codex, &home).map(|h| h.plan),
            Some("# Plan\n- a".into())
        );
        let chat = json!({"last_assistant_message": "Which database?"});
        assert_eq!(handoff(Agent::Codex, &chat, &home), None);

        let plan_dir = home.join(".grok/sessions/%2Fx/g1");
        let _ = fs::create_dir_all(&plan_dir);
        let _ = fs::write(plan_dir.join("plan.md"), "# Grok plan");
        let grok = json!({"sessionId": "g1", "toolName": "exit_plan_mode", "toolInput": {}});
        let found = handoff(Agent::Grok, &grok, &home);
        assert_eq!(found.as_ref().map(|h| h.plan.as_str()), Some("# Grok plan"));
        assert_eq!(found.and_then(|h| h.file), Some(plan_dir.join("plan.md")));

        let artifacts = home.join("brain/a1");
        let _ = fs::create_dir_all(&artifacts);
        let agy = json!({"conversationId": "a1", "artifactDirectoryPath": artifacts});
        assert_eq!(handoff(Agent::Antigravity, &agy, &home), None);
        let _ = fs::write(artifacts.join("implementation_plan.md"), "# Implementation");
        assert_eq!(
            handoff(Agent::Antigravity, &agy, &home).map(|h| h.plan),
            Some("# Implementation".into())
        );

        let cursor = json!({"conversation_id": "k", "tool_name": "CreatePlan",
            "tool_input": {"plan": "# Cursor plan", "name": "x"}});
        assert_eq!(
            handoff(Agent::Cursor, &cursor, &home).map(|h| h.plan),
            Some("# Cursor plan".into())
        );
        let shell = json!({"tool_name": "Shell", "tool_input": {"command": "ls"}});
        assert_eq!(handoff(Agent::Cursor, &shell, &home), None);
    }

    #[test]
    fn a_plan_already_reviewed_in_a_conversation_isnt_shown_again() {
        let data = scratch("seen");
        let plan = |text: &str| Handoff {
            plan: text.into(),
            file: None,
            session: "s/1".into(),
            input: Value::Null,
        };
        assert!(!seen_before(Agent::Codex, &plan("one"), &data));
        remember(Agent::Codex, &plan("one"), &data);
        assert!(seen_before(Agent::Codex, &plan("one"), &data));
        assert!(!seen_before(Agent::Codex, &plan("two"), &data));
        assert!(!seen_before(Agent::Antigravity, &plan("two"), &data));
    }

    #[test]
    fn plans_without_a_file_are_kept_one_per_conversation() {
        let data = scratch("files");
        let handoff = Handoff {
            plan: "# P".into(),
            file: Some(PathBuf::from("/nowhere/plan.md")),
            session: "abc".into(),
            input: Value::Null,
        };
        let path = review_file(Agent::Codex, &handoff, &data).unwrap_or_default();
        assert_eq!(path, data.join("plans/codex-abc.md"));
        assert_eq!(fs::read_to_string(&path).ok().as_deref(), Some("# P"));
        let own = data.join("own.md");
        let _ = fs::write(&own, "# P");
        let with_file = Handoff {
            file: Some(own.clone()),
            ..handoff
        };
        assert_eq!(
            review_file(Agent::Claude, &with_file, &data).ok(),
            Some(own)
        );
    }

    #[test]
    fn each_agent_hears_the_verdict_in_its_own_format() {
        let input = json!({"plan": "p"});
        assert_eq!(
            answer(Agent::Claude, &sent(Verdict::Approve), &input),
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "allow", "updatedInput": {"plan": "p"}}}})
            )
        );
        assert_eq!(
            answer(Agent::Claude, &sent(Verdict::RequestChanges), &input),
            Some(
                json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": "# Review"}}})
            )
        );
        assert_eq!(
            answer(Agent::Grok, &sent(Verdict::RequestChanges), &input),
            Some(json!({"decision": "deny", "reason": "# Review"}))
        );
        assert_eq!(
            answer(Agent::Grok, &sent(Verdict::ApproveWithNotes), &input),
            Some(json!({"decision": "allow"}))
        );
        assert_eq!(
            answer(Agent::Cursor, &sent(Verdict::RequestChanges), &input)
                .map(|a| a["permission"].clone()),
            Some(json!("deny"))
        );
        assert_eq!(answer(Agent::Codex, &sent(Verdict::Approve), &input), None);
        assert_eq!(
            answer(Agent::Codex, &sent(Verdict::ApproveWithNotes), &input),
            Some(json!({"decision": "block", "reason": "# Review"}))
        );
        assert_eq!(
            answer(Agent::Antigravity, &sent(Verdict::RequestChanges), &input),
            Some(json!({"decision": "continue", "reason": "# Review"}))
        );
        for agent in Agent::ALL {
            assert_eq!(answer(agent, &Outcome::Closed, &input), None);
        }
    }
}
