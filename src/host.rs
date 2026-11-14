//! The multiplexer leaf runs in, for handing work between the person and an
//! agent: Rex, Herdr or tmux. leaf opens a review in a pane beside the agent,
//! checks the pane is still there, and types the review into the agent's pane.
//! An agent that asks for a review needs one of the three; a person reviewing
//! by hand in any terminal doesn't.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::agent;

/// Where leaf is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Rex,
    Herdr,
    Tmux,
}

impl Host {
    /// The multiplexer this process runs in, from the variables each sets in
    /// its panes. Rex and Herdr can run inside tmux, so they're asked first.
    pub fn detect() -> Option<Host> {
        Host::from_env(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    fn from_env(var: impl Fn(&str) -> Option<String>) -> Option<Host> {
        if var("REX_SESSION").is_some() {
            Some(Host::Rex)
        } else if var("HERDR_ENV").as_deref() == Some("1") && var("HERDR_PANE_ID").is_some() {
            Some(Host::Herdr)
        } else if var("TMUX").is_some() && var("TMUX_PANE").is_some() {
            Some(Host::Tmux)
        } else {
            None
        }
    }

    /// This process's own pane.
    pub fn own_pane(&self) -> Option<String> {
        let name = match self {
            Host::Rex => "REX_BLOCK",
            Host::Herdr => "HERDR_PANE_ID",
            Host::Tmux => "TMUX_PANE",
        };
        std::env::var(name).ok().filter(|pane| !pane.is_empty())
    }

    /// Opens a pane to the right of `beside` (or this pane) running `argv`
    /// in `dir`, focused, and returns its id. The pane closes when `argv`
    /// exits.
    pub fn split_beside(
        &self,
        beside: Option<&str>,
        dir: &Path,
        label: &str,
        argv: &[String],
    ) -> Result<String, String> {
        let beside = beside.map(str::to_owned).or_else(|| self.own_pane());
        match self {
            Host::Rex => agent::split_beside(beside.as_deref(), dir, label, argv),
            Host::Tmux => {
                let mut args = vec!["split-window", "-h", "-P", "-F", "#{pane_id}"];
                let dir = dir.display().to_string();
                args.extend(["-c", &dir]);
                if let Some(pane) = &beside {
                    args.extend(["-t", pane]);
                }
                let line = shell_line(argv);
                args.push(&line);
                let printed = output("tmux", &args, None)?;
                let pane = printed.trim();
                if pane.starts_with('%') {
                    Ok(pane.to_owned())
                } else {
                    Err(format!("tmux split-window printed no pane id: {printed}"))
                }
            }
            Host::Herdr => {
                let dir = dir.display().to_string();
                let mut args = vec!["pane", "split"];
                match &beside {
                    Some(pane) => args.extend(["--pane", pane]),
                    None => args.push("--current"),
                }
                args.extend(["--direction", "right", "--cwd", &dir, "--focus"]);
                let printed = output("herdr", &args, None)?;
                let pane = herdr_pane_id(&printed)
                    .ok_or_else(|| format!("herdr pane split printed no pane id: {printed}"))?;
                let _ = output("herdr", &["pane", "rename", &pane, label], None);
                // The pane starts a shell; `exec` hands it to leaf, so the
                // pane closes when the review does.
                let line = format!("exec {}", shell_line(argv));
                output("herdr", &["pane", "run", &pane, &line], None)?;
                Ok(pane)
            }
        }
    }

    /// Whether pane `pane` is still open. When the multiplexer can't be
    /// asked, it's taken as open, so a waiter isn't cut short.
    pub fn is_open(&self, pane: &str) -> bool {
        match self {
            Host::Rex => agent::block_open(pane),
            Host::Tmux => match run("tmux", &["display-message", "-p", "-t", pane, "#{pane_id}"]) {
                Ok(out) if out.status.success() => {
                    String::from_utf8_lossy(&out.stdout).trim() == pane
                }
                Ok(_) => false,
                Err(_) => true,
            },
            Host::Herdr => match run("herdr", &["pane", "get", pane]) {
                Ok(out) => out.status.success(),
                Err(_) => true,
            },
        }
    }

    /// Pastes `text` into pane `pane` and submits it, as if typed there.
    pub fn type_into(&self, pane: &str, text: &str) -> Result<(), String> {
        match self {
            Host::Rex => agent::deliver_to(pane, text)
                .map(drop)
                .map_err(|e| e.to_string()),
            Host::Tmux => {
                let pasted = text.replace("\u{1b}[201~", "");
                static DELIVERIES: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let serial = DELIVERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let buffer = format!("leaf-margin-{}-{serial}", std::process::id());
                output("tmux", &["load-buffer", "-b", &buffer, "-"], Some(&pasted))?;
                let pasted = output(
                    "tmux",
                    &["paste-buffer", "-p", "-d", "-b", &buffer, "-t", pane],
                    None,
                );
                if pasted.is_err() {
                    let _ = output("tmux", &["delete-buffer", "-b", &buffer], None);
                }
                pasted?;
                output("tmux", &["send-keys", "-t", pane, "Enter"], None).map(drop)
            }
            Host::Herdr => {
                let pasted = format!("\u{1b}[200~{}\u{1b}[201~", text.replace("\u{1b}[201~", ""));
                output("herdr", &["pane", "send-text", pane, &pasted], None)?;
                output("herdr", &["pane", "send-keys", pane, "Enter"], None).map(drop)
            }
        }
    }

    /// An agent pane in this window, other than this one, and the agent in it.
    pub fn find_agent(&self) -> Option<(String, String)> {
        let own = self.own_pane();
        match self {
            Host::Rex => None,
            Host::Herdr => {
                let listed = output("herdr", &["pane", "list"], None).ok()?;
                herdr_agents(&listed)
                    .into_iter()
                    .find(|(pane, _)| Some(pane) != own.as_ref())
            }
            Host::Tmux => {
                let listed = output(
                    "tmux",
                    &["list-panes", "-F", "#{pane_id}\t#{pane_pid}"],
                    None,
                )
                .ok()?;
                let processes = output("ps", &["-A", "-o", "pid=,ppid=,stat=,args="], None).ok()?;
                tmux_agents(&listed, &processes)
                    .into_iter()
                    .find(|(pane, _)| Some(pane) != own.as_ref())
            }
        }
    }
}

fn run(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Runs `program`, feeding it `stdin`, and returns what it printed.
fn output(program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let failed =
        |cause: String| format!("{program} {} failed: {cause}", args.first().unwrap_or(&""));
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| failed(e.to_string()))?;
    if let (Some(text), Some(mut input)) = (stdin, child.stdin.take()) {
        input
            .write_all(text.as_bytes())
            .map_err(|e| failed(e.to_string()))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| failed(e.to_string()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(failed(if err.is_empty() {
            out.status.to_string()
        } else {
            err
        }))
    }
}

/// `argv` as one line for a POSIX shell or fish, each word single-quoted.
pub fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| format!("'{}'", word.replace('\'', r"'\''")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The new pane's id in `herdr pane split`'s JSON reply.
fn herdr_pane_id(printed: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(printed).ok()?;
    [
        "/result/pane/pane_id",
        "/pane/pane_id",
        "/result/pane_id",
        "/pane_id",
    ]
    .iter()
    .find_map(|at| value.pointer(at).and_then(|id| id.as_str()))
    .map(str::to_owned)
}

/// Every pane in `herdr pane list`'s JSON reply that Herdr knows runs an
/// agent, with that agent's name.
fn herdr_agents(printed: &str) -> Vec<(String, String)> {
    fn walk(value: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::Object(map) => {
                let pane = map.get("pane_id").and_then(|id| id.as_str());
                let agent = map
                    .get("agent")
                    .and_then(|agent| agent.as_str())
                    .filter(|agent| !agent.is_empty());
                if let (Some(pane), Some(agent)) = (pane, agent) {
                    out.push((pane.to_owned(), agent.to_owned()));
                }
                map.values().for_each(|v| walk(v, out));
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(printed) {
        walk(&value, &mut out);
    }
    out
}

/// The agent named by a process's command line: the executable itself, or
/// the script an interpreter was given. Other arguments don't count, so
/// `vim /usr/local/bin/claude` isn't taken for Claude.
fn agent_in_args(args: &str) -> Option<&'static str> {
    let mut words = args.split_whitespace();
    let exe = words.next()?;
    if let Some(name) = agent::agent_name(exe) {
        return Some(name);
    }
    let tag = Path::new(exe).file_name()?.to_str()?.to_ascii_lowercase();
    let interpreter = matches!(
        tag.as_str(),
        "node" | "bun" | "deno" | "python" | "python3" | "sh" | "bash" | "zsh"
    );
    interpreter
        .then(|| words.take(3).find_map(agent::agent_name))
        .flatten()
}

/// The panes in `list-panes` output (`%id<TAB>pid`) that have an agent CLI
/// running under them, found in `ps -A -o pid=,ppid=,stat=,args=` output.
fn tmux_agents(panes: &str, processes: &str) -> Vec<(String, String)> {
    let procs: Vec<(u32, u32, &str)> = processes
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            let ppid = parts.next()?.parse().ok()?;
            // A stopped process (Ctrl+Z) doesn't own the terminal, so the
            // shell under it would get the paste instead.
            let state = parts.next()?;
            if state.starts_with('T') {
                return None;
            }
            let args = line.trim_start();
            let args = args[args.find(char::is_whitespace)?..].trim_start();
            let args = args[args.find(char::is_whitespace)?..].trim_start();
            let args = args[args.find(char::is_whitespace)?..].trim_start();
            Some((pid, ppid, args))
        })
        .collect();
    // The pane's process and everything under it, nearest first.
    let agent_under = |root: u32| {
        let mut queue = vec![root];
        let mut visited = 0;
        while let Some(pid) = queue.pop() {
            visited += 1;
            if visited > 4096 {
                break;
            }
            for (child, parent, args) in &procs {
                if *child == pid
                    && let Some(name) = agent_in_args(args)
                {
                    return Some(name.to_owned());
                }
                if *parent == pid && *child != pid {
                    queue.insert(0, *child);
                }
            }
        }
        None
    };
    panes
        .lines()
        .filter_map(|line| {
            let (pane, pid) = line.split_once('\t')?;
            let name = agent_under(pid.trim().parse().ok()?)?;
            Some((pane.to_owned(), name))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_host_comes_from_the_panes_variables() {
        assert_eq!(Host::from_env(env(&[])), None);
        assert_eq!(
            Host::from_env(env(&[("TMUX", "/tmp/s,1,0"), ("TMUX_PANE", "%3")])),
            Some(Host::Tmux)
        );
        assert_eq!(
            Host::from_env(env(&[
                ("TMUX", "/tmp/s,1,0"),
                ("TMUX_PANE", "%3"),
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w1:p2"),
            ])),
            Some(Host::Herdr)
        );
        assert_eq!(
            Host::from_env(env(&[("HERDR_ENV", "1"), ("REX_SESSION", "s")])),
            Some(Host::Rex)
        );
        assert_eq!(Host::from_env(env(&[("TMUX", "/tmp/s,1,0")])), None);
    }

    #[test]
    fn commands_are_quoted_for_any_shell() {
        let argv = ["/opt/my leaf/leaf", "plan", "it's.md"].map(str::to_owned);
        assert_eq!(
            shell_line(&argv),
            r"'/opt/my leaf/leaf' 'plan' 'it'\''s.md'"
        );
    }

    #[test]
    fn herdr_replies_give_pane_ids_and_agents() {
        assert_eq!(
            herdr_pane_id(r#"{"result": {"pane": {"pane_id": "w1:p4"}}}"#),
            Some("w1:p4".into())
        );
        assert_eq!(herdr_pane_id("not json"), None);
        let listed = r#"{"result": {"panes": [
            {"pane_id": "w1:p1", "agent": "claude", "agent_status": "idle"},
            {"pane_id": "w1:p2", "agent": null},
            {"pane_id": "w1:p3", "agent": ""}
        ]}}"#;
        assert_eq!(
            herdr_agents(listed),
            vec![("w1:p1".to_owned(), "claude".to_owned())]
        );
    }

    #[test]
    fn tmux_panes_with_an_agent_under_them_are_found() {
        let panes = "%1\t100\n%2\t200\n%3\t300\n%4\t400\n";
        let processes = "  100     1 Ss   -zsh
  101   100 S+   node /opt/homebrew/bin/codex
  200     1 Ss   -fish
  201   200 S+   vim /usr/local/bin/claude
  300     1 Ss   /bin/zsh
  301   300 S+   /Users/me/.local/bin/claude --resume
  400     1 Ss   /bin/zsh
  401   400 T    /Users/me/.local/bin/claude
";
        assert_eq!(
            tmux_agents(panes, processes),
            vec![
                ("%1".to_owned(), "codex".to_owned()),
                ("%3".to_owned(), "claude".to_owned())
            ]
        );
    }
}
