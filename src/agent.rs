//! Finding an agent's tmux pane and pasting a review comment into its prompt.
//!
//! Pasting is bracketed (`tmux paste-buffer -p`), so the agent receives a paste
//! event and does not submit the prompt. The reader still decides when to send.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
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
];

#[derive(Debug, thiserror::Error)]
pub enum AgentFailed {
    #[error(
        "Not running inside tmux, so there is no agent pane to paste into. The comment was copied to the clipboard instead."
    )]
    NoTmux,
    #[error(
        "No agent pane (claude, codex, opencode, gemini, aider…) is open in tmux. The comment was copied to the clipboard instead. Set LEAF_AGENT_PANE=%3 to name one."
    )]
    NoAgent,
    #[error(
        "The pane {pane} named by LEAF_AGENT_PANE isn't open. Fix or unset LEAF_AGENT_PANE, then try again. The comment was copied to the clipboard instead."
    )]
    BadPane { pane: String },
    #[error(
        "Could not paste into the agent pane {pane} ({cause}). The comment was copied to the clipboard instead."
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

/// The best agent pane in this tmux session, preferring this window, the
/// active pane, and the most recently used one.
pub fn find() -> Result<Agent, AgentFailed> {
    if std::env::var_os("TMUX").is_none() {
        return Err(AgentFailed::NoTmux);
    }
    if let Some(pane) = std::env::var("LEAF_AGENT_PANE")
        .ok()
        .map(|pane| pane.trim().to_owned())
        .filter(|pane| !pane.is_empty())
    {
        let output = tmux(
            &[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{pane_id}\t#{pane_current_command}",
            ],
            None,
        )
        .map_err(|_| AgentFailed::BadPane { pane: pane.clone() })?;
        if !output.status.success() {
            return Err(AgentFailed::BadPane { pane });
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut parts = text.trim().splitn(2, '\t');
        let id = parts.next().unwrap_or(&pane);
        let command = parts.next().unwrap_or("agent");
        return Ok(Agent {
            pane: id.to_owned(),
            command: command.to_owned(),
        });
    }

    let listed = tmux(
        &[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{session_name}\t#{window_index}\t#{pane_active}\t#{pane_pid}\t#{pane_activity}",
        ],
        None,
    )
    .map_err(|e| AgentFailed::Tmux {
        pane: "tmux".to_owned(),
        cause: e,
    })?;
    if !listed.status.success() {
        return Err(AgentFailed::NoTmux);
    }
    let panes = parse_panes(&String::from_utf8_lossy(&listed.stdout));
    if panes.is_empty() {
        return Err(AgentFailed::NoAgent);
    }

    let current = tmux(
        &[
            "display-message",
            "-p",
            "#{session_name}\t#{window_index}\t#{pane_id}",
        ],
        None,
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let (session, window, pane_id) = match current.as_deref() {
        Some(text) => {
            let mut parts = text.split('\t');
            (
                parts.next().unwrap_or("").to_owned(),
                parts.next().unwrap_or("").to_owned(),
                parts.next().unwrap_or("").to_owned(),
            )
        }
        None => (String::new(), String::new(), String::new()),
    };

    let processes = processes();
    let own = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut best: Option<(u8, bool, u64, Agent)> = None;
    for pane in panes {
        if pane.id == own {
            // Our own pane is never an agent.
            continue;
        }
        let Some(command) = agent_in(pane.pid, &processes) else {
            continue;
        };
        let tier = if pane.id == pane_id {
            3
        } else if pane.session == session && pane.window == window {
            2
        } else if pane.session == session {
            1
        } else {
            0
        };
        let candidate = Agent {
            pane: pane.id.clone(),
            command,
        };
        let better = match &best {
            None => true,
            Some((best_tier, best_active, best_activity, _)) => {
                (tier, pane.active, pane.activity) > (*best_tier, *best_active, *best_activity)
            }
        };
        if better {
            best = Some((tier, pane.active, pane.activity, candidate));
        }
    }
    best.map(|(_, _, _, agent)| agent)
        .ok_or(AgentFailed::NoAgent)
}

/// Pastes `text` into the agent's prompt without pressing enter.
pub fn deliver(text: &str) -> Result<Agent, AgentFailed> {
    let agent = find()?;
    let buffer = format!("leaf-{}", std::process::id());
    let loaded =
        tmux(&["load-buffer", "-b", &buffer, "-"], Some(text.as_bytes())).map_err(|cause| {
            AgentFailed::Tmux {
                pane: agent.pane.clone(),
                cause,
            }
        })?;
    if !loaded.status.success() {
        return Err(AgentFailed::Tmux {
            pane: agent.pane.clone(),
            cause: stderr(&loaded),
        });
    }
    let pasted = tmux(
        &["paste-buffer", "-p", "-b", &buffer, "-t", &agent.pane],
        None,
    );
    let _ = tmux(&["delete-buffer", "-b", &buffer], None);
    match pasted {
        Ok(output) if output.status.success() => Ok(agent),
        Ok(output) => Err(AgentFailed::Tmux {
            pane: agent.pane,
            cause: stderr(&output),
        }),
        Err(cause) => Err(AgentFailed::Tmux {
            pane: agent.pane,
            cause,
        }),
    }
}

fn stderr(output: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if text.is_empty() {
        format!("tmux exited with {}", output.status)
    } else {
        text
    }
}

fn tmux(args: &[&str], input: Option<&[u8]>) -> Result<std::process::Output, String> {
    let mut command = Command::new("tmux");
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input).map_err(|e| e.to_string())?;
    }
    child.wait_with_output().map_err(|e| e.to_string())
}

#[derive(Debug)]
struct Pane {
    id: String,
    session: String,
    window: String,
    active: bool,
    pid: u32,
    activity: u64,
}

fn parse_panes(text: &str) -> Vec<Pane> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(Pane {
                id: parts.next()?.to_owned(),
                session: parts.next()?.to_owned(),
                window: parts.next()?.to_owned(),
                active: parts.next()? == "1",
                pid: parts.next()?.parse().ok()?,
                activity: parts.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            })
        })
        .collect()
}

#[derive(Debug)]
struct Process {
    parent: u32,
    words: Vec<String>,
}

fn processes() -> HashMap<u32, Process> {
    let Ok(output) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .stdin(Stdio::null())
        .output()
    else {
        return HashMap::new();
    };
    let mut processes = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut words = line.split_whitespace();
        let (Some(pid), Some(parent)) = (
            words.next().and_then(|v| v.parse::<u32>().ok()),
            words.next().and_then(|v| v.parse::<u32>().ok()),
        ) else {
            continue;
        };
        processes.insert(
            pid,
            Process {
                parent,
                words: words.map(str::to_owned).collect(),
            },
        );
    }
    processes
}

/// The agent command running under a pane's shell, if any.
fn agent_in(pane_pid: u32, processes: &HashMap<u32, Process>) -> Option<String> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, process) in processes {
        children.entry(process.parent).or_default().push(*pid);
    }
    let mut seen = HashSet::new();
    let mut stack = vec![pane_pid];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(process) = processes.get(&pid) {
            for word in &process.words {
                if let Some(name) = agent_name(word) {
                    return Some(name.to_owned());
                }
            }
        }
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids.iter().copied());
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_parse_tab_separated_fields() {
        let panes = parse_panes("%1\tmain\t0\t1\t4321\t1700000000\n%2\tmain\t1\t0\t5555\t\n");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].id, "%1");
        assert!(panes[0].active);
        assert_eq!(panes[0].pid, 4321);
        assert_eq!(panes[1].window, "1");
        assert_eq!(panes[1].activity, 0);
    }

    #[test]
    fn an_agent_behind_a_runtime_is_found() {
        let mut processes = HashMap::new();
        processes.insert(
            10,
            Process {
                parent: 1,
                words: vec!["-zsh".into()],
            },
        );
        processes.insert(
            11,
            Process {
                parent: 10,
                words: vec!["node".into(), "/opt/claude".into()],
            },
        );
        processes.insert(
            12,
            Process {
                parent: 10,
                words: vec!["git".into(), "status".into()],
            },
        );
        assert_eq!(agent_in(10, &processes).as_deref(), Some("claude"));
        assert_eq!(agent_in(12, &processes), None);
    }

    #[test]
    fn command_names_lose_their_wrappers() {
        assert_eq!(agent_name("/usr/local/bin/codex"), Some("codex"));
        assert_eq!(agent_name("opencode.exe"), Some("opencode"));
        assert_eq!(agent_name("node"), None);
        assert_eq!(agent_name("vim"), None);
    }
}
