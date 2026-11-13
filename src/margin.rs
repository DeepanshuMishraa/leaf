//! Running a Margin round: where the review screen opens (here, or in a Rex
//! pane beside the agent that asked), and where the review goes when it's
//! sent (that agent, stdout, or a result file a waiting process reads).

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::agent;
use crate::planview::{self, Plan};
use crate::review::{Outcome, Verdict};

/// Where a sent review goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Into an agent pane: the one named, or the one the usual lookup finds.
    Agent(Option<String>),
    /// To stdout, after the screen closes.
    Print,
    /// Into a file that a waiting `leaf plan --wait` reads.
    Result(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum MarginFailed {
    #[error(
        "leaf can't show the review here: this isn't a terminal and not a Rex pane, so there's nowhere to open it. Ask the person to run `{command}` in their terminal; the review will come back to you as a message."
    )]
    Nowhere { command: String },
    #[error("Opening the review pane beside the agent failed ({0}). Nothing was sent.")]
    Split(String),
    #[error("Couldn't read the plan {path} ({cause}). Check the path and try again.")]
    Read { path: String, cause: std::io::Error },
    #[error("The plan {path} is empty, so there's nothing to review.")]
    Empty { path: String },
    #[error(
        "Couldn't write the review to {path} ({cause}). The review is printed below so it isn't lost."
    )]
    WriteResult { path: String, cause: std::io::Error },
    #[error("The review screen failed ({0}). Nothing was sent.")]
    Terminal(std::io::Error),
}

/// What `leaf plan` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanRequest {
    file: String,
    reply: Reply,
    /// Block until the review is sent and print it, instead of returning at once.
    wait: bool,
}

const PLAN_USAGE: &str = "Usage: leaf plan [--wait | --print] [--reply-to <rex block>] <plan.md>

Opens a Markdown plan for review, block by block. The review goes to the agent
pane that asked (--reply-to, or the one leaf finds), or to stdout with --print.

Run by an agent (no terminal) inside Rex, it opens beside the agent and returns
at once; the review arrives as the agent's next message. --wait blocks until
the review is sent and prints it instead.";

fn parse_plan(args: &[String]) -> Result<PlanRequest, String> {
    let mut file = None;
    let mut reply = Reply::Agent(None);
    let mut wait = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--wait" => wait = true,
            "--print" => reply = Reply::Print,
            "--reply-to" => {
                let block = args.next().ok_or("--reply-to needs a Rex block id")?;
                reply = Reply::Agent(Some(block.clone()));
            }
            "--result" => {
                let path = args.next().ok_or("--result needs a file path")?;
                reply = Reply::Result(PathBuf::from(path));
            }
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown option {other}. Run `leaf plan --help` for usage."
                ));
            }
            other => {
                if file.replace(other.to_owned()).is_some() {
                    return Err("leaf plan reviews one plan at a time.".into());
                }
            }
        }
    }
    let file = file.ok_or("name the plan to review: leaf plan <plan.md>")?;
    Ok(PlanRequest { file, reply, wait })
}

/// `leaf plan`.
pub fn plan_command(args: &[String]) -> ExitCode {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{PLAN_USAGE}");
        return ExitCode::SUCCESS;
    }
    let request = match parse_plan(args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("leaf: {message}");
            return ExitCode::from(2);
        }
    };
    let source = match read_plan(Path::new(&request.file)) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    if interactive {
        let plan = Plan {
            subject: request.file.clone(),
            source,
            round: None,
        };
        return match planview::run(plan) {
            Ok(outcome) => finish(&outcome, &request.reply),
            Err(e) => {
                eprintln!("leaf: {}", MarginFailed::Terminal(e));
                ExitCode::from(1)
            }
        };
    }
    let command = format!("leaf plan {}", request.file);
    match beside(
        &["plan".to_owned(), request.file.clone()],
        request.wait,
        &command,
    ) {
        Ok(Opened::Returned) => {
            println!(
                "Opened {} for review in a pane beside you. The review arrives as your next message: end your turn now and don't poll for it.",
                request.file
            );
            ExitCode::SUCCESS
        }
        Ok(Opened::Waited(outcome)) => {
            print_outcome(&outcome);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("leaf: {e}");
            ExitCode::from(2)
        }
    }
}

pub fn read_plan(path: &Path) -> Result<String, MarginFailed> {
    let source = std::fs::read_to_string(path).map_err(|cause| MarginFailed::Read {
        path: path.display().to_string(),
        cause,
    })?;
    if source.trim().is_empty() {
        return Err(MarginFailed::Empty {
            path: path.display().to_string(),
        });
    }
    Ok(source)
}

/// How a review opened beside the agent went, from the caller's side.
pub enum Opened {
    /// The pane is open; the review will reach the agent on its own.
    Returned,
    /// The caller waited, and this is how the review ended.
    Waited(Outcome),
}

/// Opens `leaf <args>` in a Rex pane beside the agent that ran this. Without
/// `wait`, the pane sends the review to that agent itself; with it, the pane
/// writes a result file that this process waits for and reads.
pub fn beside(args: &[String], wait: bool, command: &str) -> Result<Opened, MarginFailed> {
    let nowhere = || MarginFailed::Nowhere {
        command: command.to_owned(),
    };
    let Some(_session) = agent::rex_session_id() else {
        return Err(nowhere());
    };
    let asking = std::env::var("REX_BLOCK").ok().filter(|b| !b.is_empty());
    let exe = std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "leaf".into());
    let (verb, rest) = args.split_first().ok_or_else(nowhere)?;
    let mut argv = vec![exe, verb.clone()];
    let result = wait.then(result_path);
    match (&result, &asking) {
        (Some(path), _) => argv.extend(["--result".into(), path.display().to_string()]),
        (None, Some(block)) => argv.extend(["--reply-to".into(), block.clone()]),
        (None, None) => {}
    }
    argv.extend(rest.iter().cloned());
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pane = agent::split_beside(asking.as_deref(), &dir, "review", &argv)
        .map_err(MarginFailed::Split)?;
    match result {
        None => Ok(Opened::Returned),
        Some(path) => Ok(Opened::Waited(wait_for(&path, &pane))),
    }
}

fn margin_dir() -> PathBuf {
    crate::progress::default_path().with_file_name("margin")
}

/// A fresh file for one round's result.
fn result_path() -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    margin_dir().join(format!("result-{}-{stamp}.json", std::process::id()))
}

/// How often a waiter looks for the result, and how often it checks the pane
/// is still there.
const POLL: Duration = Duration::from_millis(250);
const PANE_CHECK: Duration = Duration::from_secs(2);

/// Waits for the review pane's result. A pane closed without one counts as
/// closed without a verdict.
pub fn wait_for(path: &Path, pane: &str) -> Outcome {
    let mut checked = Instant::now();
    loop {
        if let Some(outcome) = read_result(path) {
            let _ = std::fs::remove_file(path);
            return outcome;
        }
        if checked.elapsed() >= PANE_CHECK {
            checked = Instant::now();
            if !agent::block_open(pane) {
                // One last look: the pane may have written it as it closed.
                let outcome = read_result(path).unwrap_or(Outcome::Closed);
                let _ = std::fs::remove_file(path);
                return outcome;
            }
        }
        std::thread::sleep(POLL);
    }
}

fn outcome_json(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Sent { verdict, text } => serde_json::json!({
            "outcome": "sent",
            "verdict": verdict.as_str(),
            "text": text,
        }),
        Outcome::Closed => serde_json::json!({ "outcome": "closed" }),
    }
    .to_string()
}

fn parse_outcome(text: &str) -> Option<Outcome> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    match value.get("outcome")?.as_str()? {
        "closed" => Some(Outcome::Closed),
        "sent" => Some(Outcome::Sent {
            verdict: Verdict::parse(value.get("verdict")?.as_str()?)?,
            text: value.get("text")?.as_str()?.to_owned(),
        }),
        _ => None,
    }
}

/// The result, once it's been written whole.
fn read_result(path: &Path) -> Option<Outcome> {
    parse_outcome(&std::fs::read_to_string(path).ok()?)
}

/// Writes the result next to its final name, then moves it there, so a
/// waiter never reads half of it.
fn write_result(path: &Path, outcome: &Outcome) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let partial = path.with_extension("partial");
    std::fs::write(&partial, outcome_json(outcome))?;
    std::fs::rename(&partial, path)
}

fn print_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Sent { text, .. } => print!("{text}"),
        Outcome::Closed => println!(
            "The review was closed without a verdict. Ask the person how they'd like to go on."
        ),
    }
}

fn copy_to_clipboard(text: &str) {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{encoded}\x07").and_then(|_| out.flush());
}

/// Holds a pane that's about to close until the person has read why.
fn hold_open() {
    eprintln!("Press enter to close this pane.");
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
}

/// Sends a finished review where it was meant to go.
pub fn finish(outcome: &Outcome, reply: &Reply) -> ExitCode {
    match reply {
        Reply::Result(path) => match write_result(path, outcome) {
            Ok(()) => ExitCode::SUCCESS,
            Err(cause) => {
                eprintln!(
                    "leaf: {}",
                    MarginFailed::WriteResult {
                        path: path.display().to_string(),
                        cause,
                    }
                );
                print_outcome(outcome);
                hold_open();
                ExitCode::from(1)
            }
        },
        Reply::Print => {
            print_outcome(outcome);
            ExitCode::SUCCESS
        }
        Reply::Agent(pane) => {
            let Outcome::Sent { text, verdict } = outcome else {
                return ExitCode::SUCCESS;
            };
            let delivered = match pane {
                Some(pane) => agent::deliver_to(pane, text),
                None => agent::deliver(text),
            };
            match delivered {
                Ok(found) => {
                    eprintln!(
                        "leaf: review ({}) sent to {}",
                        verdict.label(),
                        found.describe()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    copy_to_clipboard(text);
                    println!("{text}");
                    eprintln!("leaf: {e}");
                    if pane.is_some() {
                        hold_open();
                    }
                    ExitCode::from(1)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn plan_arguments_pick_where_the_review_goes() {
        assert_eq!(
            parse_plan(&args(&["plan.md"])),
            Ok(PlanRequest {
                file: "plan.md".into(),
                reply: Reply::Agent(None),
                wait: false
            })
        );
        assert_eq!(
            parse_plan(&args(&["--reply-to", "b7", "plan.md"])).map(|r| r.reply),
            Ok(Reply::Agent(Some("b7".into())))
        );
        assert_eq!(
            parse_plan(&args(&["--print", "plan.md"])).map(|r| r.reply),
            Ok(Reply::Print)
        );
        assert_eq!(
            parse_plan(&args(&["--wait", "plan.md"])).map(|r| r.wait),
            Ok(true)
        );
        assert!(parse_plan(&args(&[])).is_err());
        assert!(parse_plan(&args(&["a.md", "b.md"])).is_err());
        assert!(parse_plan(&args(&["--nope", "a.md"])).is_err());
        assert!(parse_plan(&args(&["--reply-to"])).is_err());
    }

    #[test]
    fn a_result_file_round_trips_every_outcome() {
        let dir = std::env::temp_dir().join(format!("leaf-margin-{}", std::process::id()));
        let path = dir.join("result.json");
        for outcome in [
            Outcome::Closed,
            Outcome::Sent {
                verdict: Verdict::RequestChanges,
                text: "# Review\n\n\"quoted\"\n".into(),
            },
        ] {
            write_result(&path, &outcome).expect("writes");
            assert_eq!(read_result(&path), Some(outcome));
        }
        assert!(!path.with_extension("partial").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_half_written_or_foreign_result_is_not_read() {
        assert_eq!(parse_outcome("{\"outcome\":\"sent\""), None);
        assert_eq!(
            parse_outcome("{\"outcome\":\"sent\",\"verdict\":\"maybe\",\"text\":\"\"}"),
            None
        );
        assert_eq!(parse_outcome("{\"outcome\":\"other\"}"), None);
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let path = std::env::temp_dir().join(format!("leaf-empty-plan-{}.md", std::process::id()));
        std::fs::write(&path, "\n  \n").expect("writes");
        assert!(matches!(read_plan(&path), Err(MarginFailed::Empty { .. })));
        let _ = std::fs::remove_file(path);
        assert!(matches!(
            read_plan(Path::new("/nowhere/plan.md")),
            Err(MarginFailed::Read { .. })
        ));
    }
}
