//! Finding an agent pane in this Rex session and sending it a prompt.
//!
//! The text is bracketed-pasted, then Enter is pressed outside the paste, so
//! the agent receives the whole prompt and submits it.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use std::process::{Command, Stdio};

/// Commands that are agent CLIs, even when launched through a runtime.
const AGENTS: &[&str] = &[
    "claude",
    "claude-code",
    "codex",
    "opencode",
    "open-code",
    "gemini",
    "aider",
    "cursor-agent",
    "amp",
    "goose",
    "crush",
    "droid",
    "qwen",
    "kilo",
    "cline",
    "roo",
    "windsurf",
    "copilot",
    "auggie",
    "grok",
];

#[derive(Debug, thiserror::Error)]
pub enum AgentFailed {
    #[error(
        "Not running inside Rex, so there is no agent pane to send to. The text was copied to the clipboard instead."
    )]
    NoTmux,
    #[error(
        "No agent pane (claude, codex, opencode, gemini, grok, aider…) is open in this Rex session. The text was copied to the clipboard instead. Set LEAF_AGENT_PANE to a Rex block id."
    )]
    NoAgent,
    #[error(
        "The pane {pane} named by LEAF_AGENT_PANE isn't open. Fix or unset LEAF_AGENT_PANE, then try again. The text was copied to the clipboard instead."
    )]
    BadPane { pane: String },
    #[error(
        "Could not send to the agent pane {pane} ({cause}). The text was copied to the clipboard instead."
    )]
    Tmux { pane: String, cause: String },
}

/// An agent pane the comment can go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub pane: String,
    pub command: String,
}

impl Agent {
    pub fn describe(&self) -> String {
        format!("{} in {}", self.command, self.pane)
    }
}

/// The agent block to send to: one already open in this Rex session.
pub fn find() -> Result<Agent, AgentFailed> {
    let Some(session) = rex_session() else {
        return Err(AgentFailed::NoTmux);
    };
    find_rex(&session)
}

/// Sends `text` to the agent: a bracketed paste, then Enter outside it.
pub fn deliver(text: &str) -> Result<Agent, AgentFailed> {
    deliver_rex(text)
}

fn stderr(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        format!("rex exited with {}", output.status)
    } else {
        text
    }
}

fn agent_name(word: &str) -> Option<&'static str> {
    let name = Path::new(word).file_name()?.to_str()?.to_ascii_lowercase();
    let name = name
        .strip_suffix(".js")
        .or_else(|| name.strip_suffix(".mjs"))
        .or_else(|| name.strip_suffix(".cjs"))
        .or_else(|| name.strip_suffix(".exe"))
        .unwrap_or(&name);
    AGENTS.iter().find(|agent| **agent == name).copied()
}

/// How many lines before and after the selection the agent also sees.
const AROUND: usize = 6;
/// A single line longer than this is cut, so a wrapped paragraph stays readable.
const LINE_CAP: usize = 400;

/// One page, or a document without pages, and the character range selected in it.
pub struct Excerpt {
    /// 1-based page, when the document has pages.
    pub page: Option<usize>,
    /// The page's text. `start` and `end` are character offsets into it.
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// The document, where the selection sits, and the lines around it.
/// An empty `question` leaves the question off, so a caller can add one later.
pub fn prompt(title: &str, source: Option<&str>, excerpts: &[Excerpt], question: &str) -> String {
    let label = document_label(title, source);
    let rendered: Vec<(String, String)> = excerpts.iter().map(render_excerpt).collect();
    let body = if rendered.len() == 1 {
        let (place, text) = &rendered[0];
        format!(
            "From {label}, {place}. Lines marked >> are the selection. The others are the text around it.\n\n{text}\n"
        )
    } else if rendered.is_empty() {
        format!("From {label}.\n")
    } else {
        let parts = rendered
            .iter()
            .map(|(place, text)| format!("{place}\n\n{text}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        format!(
            "From {label}. Lines marked >> are the selection. The others are the text around it.\n\n{parts}\n"
        )
    };
    let question = question.trim();
    if question.is_empty() {
        body
    } else {
        with_question(&body, question)
    }
}

/// Adds `question`, or a short default when it is empty, and keeps the context above it.
pub fn with_question(context: &str, question: &str) -> String {
    let question = match question.trim() {
        "" => "Explain this, using the surrounding text.",
        question => question,
    };
    let mut out = context.trim_end().to_owned();
    out.push_str("\n\n");
    out.push_str(question);
    out.push('\n');
    out
}

fn document_label(title: &str, source: Option<&str>) -> String {
    let title = title.trim();
    let source = source.map(str::trim).filter(|path| !path.is_empty());
    match (source, title) {
        (Some(path), "") => path.to_owned(),
        (Some(path), title) => {
            let stem = Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("");
            if stem == title || path.ends_with(title) {
                path.to_owned()
            } else {
                format!("{path} ({title})")
            }
        }
        (None, "") => "this document".to_owned(),
        (None, title) => title.to_owned(),
    }
}

struct SrcLine {
    number: usize,
    start: usize,
    end: usize,
    text: String,
}

fn lines_of(text: &str) -> Vec<SrcLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut number = 1;
    let mut buf = String::new();
    for (index, c) in text.chars().enumerate() {
        if c == '\n' {
            lines.push(SrcLine {
                number,
                start,
                end: index,
                text: std::mem::take(&mut buf),
            });
            number += 1;
            start = index + 1;
        } else {
            buf.push(c);
        }
    }
    if start < text.chars().count() || lines.is_empty() {
        lines.push(SrcLine {
            number,
            start,
            end: text.chars().count(),
            text: buf,
        });
    }
    lines
}

fn hits(line: &SrcLine, start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    if line.start == line.end {
        line.start >= start && line.start < end
    } else {
        line.start < end && line.end > start
    }
}

fn render_excerpt(excerpt: &Excerpt) -> (String, String) {
    let lines = lines_of(&excerpt.text);
    let chosen: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| hits(line, excerpt.start, excerpt.end))
        .map(|(index, _)| index)
        .collect();
    let (first, last) = match (chosen.first(), chosen.last()) {
        (Some(first), Some(last)) => (*first, *last),
        _ => (0, lines.len().saturating_sub(1)),
    };
    let from = first.saturating_sub(AROUND);
    let to = (last + AROUND).min(lines.len().saturating_sub(1));
    let width = lines
        .get(to)
        .map(|line| line.number.to_string().len())
        .unwrap_or(1);
    let mut body = String::from("```\n");
    for line in lines.iter().take(to + 1).skip(from) {
        let mark = if hits(line, excerpt.start, excerpt.end) {
            ">>"
        } else {
            "  "
        };
        let mut shown = line.text.clone();
        if shown.chars().count() > LINE_CAP {
            shown = shown.chars().take(LINE_CAP).collect();
            shown.push('…');
        }
        body.push_str(&format!(
            "{mark} {number:>width$}| {shown}\n",
            number = line.number
        ));
    }
    body.push_str("```");
    let place = place_label(excerpt.page, lines[first].number, lines[last].number);
    (place, body)
}

fn place_label(page: Option<usize>, first: usize, last: usize) -> String {
    let lines = if first == last {
        format!("line {first}")
    } else {
        format!("lines {first}-{last}")
    };
    match page {
        Some(page) => format!("page {page}, {lines}"),
        None => lines,
    }
}

fn context_path() -> PathBuf {
    crate::progress::default_path().with_file_name("ask.json")
}

/// Remembers the last selection and the document context around it, so a later
/// `leaf ask` (the command palette) can send that context with the clipboard text.
pub fn remember(selection: &str, context: &str) {
    let selection = selection.trim();
    if selection.is_empty() {
        return;
    }
    let path = context_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = serde_json::json!({
        "selection": selection,
        "context": context,
    })
    .to_string();
    let _ = std::fs::write(path, text);
}

/// The context saved for this exact selection, when there is one.
pub fn recall(selection: &str) -> Option<String> {
    let text = std::fs::read_to_string(context_path()).ok()?;
    let saved: serde_json::Value = serde_json::from_str(&text).ok()?;
    let stored = saved.get("selection")?.as_str()?;
    let context = saved.get("context")?.as_str()?;
    (stored.trim() == selection.trim()).then(|| context.to_owned())
}

/// Tells Rex what leaf is doing, when this process is one of its panes.
/// Other terminals ignore the sequence.
pub fn report_status(state: &str, msg: &str) {
    if rex_session().is_none() {
        return;
    }
    let msg: String = msg.chars().filter(|c| !c.is_control()).take(180).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(msg);
    let kind = if state == "blocked" {
        ":kind=question"
    } else {
        ""
    };
    let mut out = std::io::stdout();
    let _ = write!(
        out,
        "\x1b]7501;state={state}{kind}:app=leaf:msg={encoded}\x1b\\"
    );
    let _ = out.flush();
}

fn rex_session() -> Option<String> {
    std::env::var("REX_SESSION")
        .ok()
        .map(|session| session.trim().to_owned())
        .filter(|session| !session.is_empty())
}

pub(crate) fn rex_program() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("LEAF_REX").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(dir) = std::env::var_os("REX_BIN_DIR").filter(|dir| !dir.is_empty()) {
        let candidate = PathBuf::from(dir).join("rex");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("rex");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let bundled = format!(
        "{home}/Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS/rex"
    );
    for candidate in [
        bundled.as_str(),
        "/Applications/Rex.app/Contents/Helpers/Rex Server.rex/Contents/MacOS/rex",
        "/Applications/Rex Beta.app/Contents/Helpers/Rex Server.rex/Contents/MacOS/rex",
    ] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(
        "rex isn't on PATH. A Rex pane has it; from anywhere else set LEAF_REX to the rex binary."
            .into(),
    )
}

fn rex_cmd(args: &[String], stdin: Option<&[u8]>) -> Result<std::process::Output, String> {
    let mut command = Command::new(rex_program()?);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    if let Some(stdin) = stdin
        && let Some(mut input) = child.stdin.take()
    {
        input.write_all(stdin).map_err(|e| e.to_string())?;
    }
    child.wait_with_output().map_err(|e| e.to_string())
}

struct RexBlock {
    id: String,
    window: String,
    creator: String,
}

fn parse_rex_blocks(text: &str) -> Vec<RexBlock> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(blocks) = value.get("blocks").and_then(|blocks| blocks.as_array()) else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            Some(RexBlock {
                id: block.get("block_id")?.as_str()?.to_owned(),
                window: block
                    .get("window_id")
                    .and_then(|window| window.as_str())
                    .unwrap_or("")
                    .to_owned(),
                creator: block
                    .get("creator_name")
                    .and_then(|creator| creator.as_str())
                    .unwrap_or("")
                    .to_owned(),
            })
        })
        .collect()
}

struct Candidate {
    id: String,
    window: String,
    command: String,
}

/// Prefers the focused agent, then one in the same window as the document.
fn best_candidate<'a>(
    candidates: &'a [Candidate],
    own_window: &str,
    focused: &str,
) -> Option<&'a Candidate> {
    let mut best: Option<(u8, &Candidate)> = None;
    for candidate in candidates {
        let tier = if candidate.id == focused {
            3
        } else if !own_window.is_empty() && candidate.window == own_window {
            2
        } else {
            1
        };
        let better = match best {
            None => true,
            Some((best_tier, _)) => tier > best_tier,
        };
        if better {
            best = Some((tier, candidate));
        }
    }
    best.map(|(_, candidate)| candidate)
}

fn agent_in_process_json(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    for key in ["foreground", "child"] {
        if let Some(name) = agent_in_proc(&value[key]) {
            return Some(name);
        }
    }
    let ancestors = value.get("ancestors")?.as_array()?;
    for proc in ancestors {
        if let Some(name) = agent_in_proc(proc) {
            return Some(name);
        }
    }
    None
}

fn agent_in_proc(proc: &serde_json::Value) -> Option<String> {
    for key in ["script", "argv0", "invoked_path", "path", "name"] {
        if let Some(word) = proc.get(key).and_then(|word| word.as_str())
            && let Some(name) = agent_name(word)
        {
            return Some(name.to_owned());
        }
    }
    None
}

/// Bracketed paste, then Enter outside the paste so the agent submits it.
fn submission(text: &str) -> String {
    let text = text.replace("\u{1b}[201~", "");
    format!("\u{1b}[200~{text}\u{1b}[201~\r")
}

fn find_rex(session: &str) -> Result<Agent, AgentFailed> {
    let listed = rex_cmd(
        &[
            "block".into(),
            "ls".into(),
            "-s".into(),
            session.into(),
            "--json".into(),
            "--short=false".into(),
        ],
        None,
    )
    .map_err(|cause| AgentFailed::Tmux {
        pane: "rex".into(),
        cause,
    })?;
    if !listed.status.success() {
        return Err(AgentFailed::Tmux {
            pane: "rex".into(),
            cause: stderr(&listed),
        });
    }
    let blocks = parse_rex_blocks(&String::from_utf8_lossy(&listed.stdout));
    let own = std::env::var("REX_BLOCK").unwrap_or_default();
    if let Some(named) = std::env::var("LEAF_AGENT_PANE")
        .ok()
        .map(|pane| pane.trim().to_owned())
        .filter(|pane| !pane.is_empty())
    {
        let Some(block) = blocks
            .iter()
            .find(|block| block.id == named || block.id.ends_with(&named))
        else {
            return Err(AgentFailed::BadPane { pane: named });
        };
        let command = agent_from_rex_process(session, &block.id).unwrap_or_else(|| "agent".into());
        return Ok(Agent {
            pane: block.id.clone(),
            command,
        });
    }
    let (_, focused) = rex_focus(session);
    let own_window = blocks
        .iter()
        .find(|block| block.id == own)
        .map(|block| block.window.clone())
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for block in &blocks {
        if block.id == own || block.creator != "com.superlogical.terminal" {
            continue;
        }
        let Some(command) = agent_from_rex_process(session, &block.id) else {
            continue;
        };
        candidates.push(Candidate {
            id: block.id.clone(),
            window: block.window.clone(),
            command,
        });
    }
    best_candidate(&candidates, &own_window, &focused)
        .map(|candidate| Agent {
            pane: candidate.id.clone(),
            command: candidate.command.clone(),
        })
        .ok_or(AgentFailed::NoAgent)
}

fn agent_from_rex_process(session: &str, block: &str) -> Option<String> {
    let output = rex_cmd(
        &[
            "block".into(),
            "call".into(),
            "-s".into(),
            session.into(),
            "-b".into(),
            block.into(),
            "process".into(),
        ],
        None,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    agent_in_process_json(&String::from_utf8_lossy(&output.stdout))
}

fn rex_focus(session: &str) -> (String, String) {
    let Ok(output) = rex_cmd(
        &[
            "session".into(),
            "inspect".into(),
            session.into(),
            "--json".into(),
        ],
        None,
    ) else {
        return (String::new(), String::new());
    };
    if !output.status.success() {
        return (String::new(), String::new());
    }
    let Ok(value) =
        serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&output.stdout))
    else {
        return (String::new(), String::new());
    };
    let window = value
        .get("active_window_id")
        .and_then(|window| window.as_str())
        .unwrap_or("")
        .to_owned();
    let focused = value
        .get("windows")
        .and_then(|windows| windows.as_array())
        .and_then(|windows| {
            windows
                .iter()
                .find(|pane| {
                    pane.get("window_id").and_then(|id| id.as_str()) == Some(window.as_str())
                })
                .and_then(|pane| pane.get("focused_block_id").and_then(|id| id.as_str()))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    (window, focused)
}

fn deliver_rex(text: &str) -> Result<Agent, AgentFailed> {
    let session = rex_session().ok_or(AgentFailed::NoTmux)?;
    let agent = find()?;
    let payload = serde_json::json!({ "data": submission(text) }).to_string();
    let output = rex_cmd(
        &[
            "block".into(),
            "call".into(),
            "-s".into(),
            session,
            "-b".into(),
            agent.pane.clone(),
            "write".into(),
            "-".into(),
        ],
        Some(payload.as_bytes()),
    )
    .map_err(|cause| AgentFailed::Tmux {
        pane: agent.pane.clone(),
        cause,
    })?;
    if output.status.success() {
        Ok(agent)
    } else {
        Err(AgentFailed::Tmux {
            pane: agent.pane,
            cause: stderr(&output),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_names_lose_their_wrappers() {
        assert_eq!(agent_name("/usr/local/bin/codex"), Some("codex"));
        assert_eq!(agent_name("opencode.exe"), Some("opencode"));
        assert_eq!(agent_name("node"), None);
        assert_eq!(agent_name("vim"), None);
    }

    #[test]
    fn a_prompt_names_the_document_and_the_lines_around_the_selection() {
        let text = "one\ntwo\nthree\nfour\nfive\n";
        let out = prompt(
            "Notes",
            Some("/docs/notes.md"),
            &[Excerpt {
                page: Some(2),
                text: text.into(),
                start: 8,
                end: 13,
            }],
            "",
        );
        assert!(out.contains("From /docs/notes.md"), "{out}");
        assert!(out.contains("page 2, line 3"), "{out}");
        assert!(out.contains(">> 3| three"), "{out}");
        assert!(out.contains("   1| one"), "{out}");
        assert!(out.contains("   5| five"), "{out}");
        assert!(!out.contains("Explain this"), "{out}");
        let asked = with_question(&out, "why?");
        assert!(asked.trim_end().ends_with("why?"), "{asked}");
        let plain = prompt(
            "",
            None,
            &[Excerpt {
                page: None,
                text: "Just this.".into(),
                start: 0,
                end: 10,
            }],
            "",
        );
        assert!(plain.contains("From this document"), "{plain}");
        assert!(plain.contains(">> 1| Just this."), "{plain}");
    }

    #[test]
    fn sending_presses_enter_after_the_paste() {
        let sent = submission("hello");
        assert_eq!(sent, "\u{1b}[200~hello\u{1b}[201~\r");
    }

    #[test]
    fn a_rex_process_tree_names_the_agent_in_front() {
        let json = r#"{"foreground":{"name":"fish","path":"/opt/homebrew/bin/fish"},"child":{"name":"login","path":"/usr/bin/login"},"ancestors":[{"name":"node","script":"/usr/local/bin/claude"}]}"#;
        assert_eq!(agent_in_process_json(json).as_deref(), Some("claude"));
        assert_eq!(
            agent_in_process_json(r#"{"foreground":{"name":"fish"}}"#).as_deref(),
            None
        );
    }

    #[test]
    fn the_focused_agent_wins_over_one_in_another_window() {
        let candidates = vec![
            Candidate {
                id: "block:other".into(),
                window: "window:a".into(),
                command: "codex".into(),
            },
            Candidate {
                id: "block:here".into(),
                window: "window:b".into(),
                command: "claude".into(),
            },
        ];
        let best = best_candidate(&candidates, "window:b", "block:other").unwrap();
        assert_eq!(best.command, "codex");
        let beside = best_candidate(&candidates, "window:b", "block:none").unwrap();
        assert_eq!(beside.command, "claude");
    }

    #[test]
    fn rex_blocks_keep_their_window() {
        let blocks = parse_rex_blocks(
            r#"{"blocks":[{"block_id":"block:1","window_id":"window:9","creator_name":"com.superlogical.terminal"},{"nope":true}]}"#,
        );
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].id, "block:1");
        assert_eq!(blocks[0].window, "window:9");
    }
}
