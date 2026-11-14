//! Running a Margin round: where the review screen opens (here, or in a Rex
//! pane beside the agent that asked), and where the review goes when it's
//! sent (that agent, stdout, or a result file a waiting process reads).

use std::cell::RefCell;
use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::Engine;

use crate::agent;
use crate::diffreview::{Margin, SaveNotes, Sends};
use crate::host::Host;
use crate::planview::{self, Plan};
use crate::progress::Store;
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
        "There's nowhere to open the review: leaf opens it in a pane beside you, which needs Rex, tmux or Herdr, and this isn't running in any of them. Ask the person to run `{command}` in their terminal, or to start you inside tmux, Herdr or Rex."
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
    /// What the review calls the plan, in place of its path.
    title: Option<String>,
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
    let mut title = None;
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
            "--title" => {
                let text = args.next().ok_or("--title needs the plan's name")?;
                title = Some(text.clone());
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
    Ok(PlanRequest {
        file,
        title,
        reply,
        wait,
    })
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
        let key = std::fs::canonicalize(&request.file)
            .unwrap_or_else(|_| PathBuf::from(&request.file))
            .display()
            .to_string();
        let (draft, save, failure) = drafts(&key);
        // A round that can't be kept only loses the comparison with the last one.
        let (round, previous) = record_round(&key, &source).unwrap_or((1, None));
        let plan = Plan {
            subject: request
                .title
                .clone()
                .unwrap_or_else(|| shown_path(&request.file)),
            source,
            round: Some(round),
            previous,
            draft,
            save,
        };
        let result = planview::run(plan);
        if let Some(message) = failure.borrow().as_ref() {
            eprintln!(
                "leaf: unsent notes couldn't be saved ({message}). They last only while the review is open."
            );
        }
        return match result {
            Ok(outcome) => finish(
                &outcome,
                if request.wait {
                    &Reply::Print
                } else {
                    &request.reply
                },
            ),
            Err(e) => {
                eprintln!("leaf: {}", MarginFailed::Terminal(e));
                ExitCode::from(1)
            }
        };
    }
    let command = format!("leaf plan {}", request.file);
    let mut forwarded = vec!["plan".to_owned()];
    if let Some(title) = &request.title {
        forwarded.extend(["--title".to_owned(), title.clone()]);
    }
    forwarded.push(request.file.clone());
    match beside(&forwarded, request.wait, &command, &request.reply) {
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

/// The unsent notes kept for the plan `key`, a saver for them, and where a
/// store failure is left to report once the screen is gone. Without a store,
/// notes last only for this run.
fn drafts(
    key: &str,
) -> (
    Option<String>,
    planview::SaveDraft,
    Rc<RefCell<Option<String>>>,
) {
    let failure = Rc::new(RefCell::new(None));
    let store = match Store::open(&crate::progress::default_path()) {
        Ok(store) => store,
        Err(e) => {
            *failure.borrow_mut() = Some(e.to_string());
            return (None, Box::new(|_| {}), failure);
        }
    };
    let draft = match store.load_margin_draft(key) {
        Ok(draft) => draft,
        Err(e) => {
            *failure.borrow_mut() = Some(e.to_string());
            None
        }
    };
    let key = key.to_owned();
    let failed = failure.clone();
    let save: planview::SaveDraft = Box::new(move |json| {
        if let Err(e) = store.save_margin_draft(&key, json) {
            *failed.borrow_mut() = Some(e.to_string());
        }
    });
    (draft, save, failure)
}

const REVIEW_USAGE: &str = "Usage: leaf review [--wait | --print] [--reply-to <rex block>] [diff options] [path | revision | GitHub url]

Reviews changes file by file: an overview of every changed file, notes on
lines (C), on a file (F) and on the whole change (M), then S sends them as one
review with a verdict. Takes the same sources as leaf diff (--staged, --branch,
--base <ref>, a range, a patch file or a GitHub link).

Run by an agent (no terminal) inside Rex, it opens beside the agent and returns
at once; the review arrives as the agent's next message. --wait blocks until
the review is sent and prints it instead.";

/// Takes Margin's own options out of `args`, leaving the rest for the diff.
fn split_review(args: &[String]) -> Result<(Reply, bool, Vec<String>), String> {
    let mut reply = Reply::Agent(None);
    let mut wait = false;
    let mut rest = Vec::new();
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
            _ => rest.push(arg.clone()),
        }
    }
    Ok((reply, wait, rest))
}

/// `leaf review`.
pub fn review_command(args: &[String]) -> ExitCode {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{REVIEW_USAGE}");
        return ExitCode::SUCCESS;
    }
    let (reply, wait, rest) = match split_review(args) {
        Ok(split) => split,
        Err(message) => {
            eprintln!("leaf: {message}");
            return ExitCode::from(2);
        }
    };
    let (request, layout) = match crate::diff_request(&rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("leaf: {message}");
            return ExitCode::from(2);
        }
    };
    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    if !interactive {
        let mut forwarded = vec!["review".to_owned()];
        forwarded.extend(rest.iter().cloned());
        let command = format!("leaf {}", forwarded.join(" "));
        return match beside(&forwarded, wait, &command, &reply) {
            Ok(Opened::Returned) => {
                println!(
                    "Opened the changes for review in a pane beside you. The review arrives as your next message: end your turn now and don't poll for it."
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
        };
    }
    std::thread::spawn(crate::syntax::warm);
    let loaded = match request.load() {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    if loaded.diff.files().is_empty() {
        eprintln!(
            "leaf: there are no changes to review in {}. Name another source, such as --staged or --branch.",
            loaded.label
        );
        return ExitCode::from(1);
    }
    let failure = Rc::new(RefCell::new(None));
    let mut options = crate::diff_options(&loaded.key, failure.clone());
    options.layout = layout;
    let result = crate::diffview::review(loaded, options);
    if let Some(message) = failure.borrow().as_ref() {
        eprintln!("leaf: {message}");
    }
    match result {
        Ok(outcome) => finish(&outcome, if wait { &Reply::Print } else { &reply }),
        Err(e) => {
            eprintln!("leaf: {}", MarginFailed::Terminal(e));
            ExitCode::from(1)
        }
    }
}

/// File and global notes for the diff `key`, kept like a plan's notes. A store
/// failure is left in `failure`, which the diff screen reports on leaving.
pub fn diff_margin(key: &str, failure: Rc<RefCell<Option<String>>>) -> Margin {
    let (draft, save, failed) = drafts(&format!("diff:{key}"));
    if let Some(message) = failed.borrow().clone() {
        *failure.borrow_mut() = Some(message);
    }
    // Later save failures surface the same way.
    let save: SaveNotes = {
        let mut inner = save;
        Box::new(move |json| {
            inner(json);
            if let Some(message) = failed.borrow().clone() {
                *failure.borrow_mut() = Some(message);
            }
        })
    };
    Margin {
        draft,
        save,
        sends: Sends::Agent,
        overview_first: false,
    }
}

/// A plan's path as the person would write it: from this folder when it's
/// under it, else from the home folder.
fn shown_path(file: &str) -> String {
    let path = Path::new(file);
    if path.is_relative() {
        return file.to_owned();
    }
    if let Some(rest) = std::env::current_dir()
        .ok()
        .and_then(|dir| path.strip_prefix(dir).ok().map(Path::to_path_buf))
    {
        return rest.display().to_string();
    }
    match std::env::var_os("HOME")
        .and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf))
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => file.to_owned(),
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
pub fn beside(
    args: &[String],
    wait: bool,
    command: &str,
    reply: &Reply,
) -> Result<Opened, MarginFailed> {
    let nowhere = || MarginFailed::Nowhere {
        command: command.to_owned(),
    };
    let host = Host::detect().ok_or_else(nowhere)?;
    let asking = host.own_pane();
    let exe = std::env::current_exe()
        .map(|exe| exe.display().to_string())
        .unwrap_or_else(|_| "leaf".into());
    let (verb, rest) = args.split_first().ok_or_else(nowhere)?;
    let mut argv = vec![exe, verb.clone()];
    // `--wait` and `--print` both need the outcome back in this process;
    // `--result` names where it goes.
    let result = match reply {
        Reply::Result(path) => Some(path.clone()),
        _ if wait || matches!(reply, Reply::Print) => Some(result_path()),
        _ => None,
    };
    match (&result, &asking, reply) {
        (Some(path), _, _) => argv.extend(["--result".into(), path.display().to_string()]),
        (None, _, Reply::Agent(Some(block))) => {
            argv.extend(["--reply-to".into(), block.clone()]);
        }
        (None, Some(block), Reply::Agent(None)) => {
            argv.extend(["--reply-to".into(), block.clone()]);
        }
        _ => {}
    }
    argv.extend(rest.iter().cloned());
    let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let pane = host
        .split_beside(asking.as_deref(), &dir, "review", &argv)
        .map_err(MarginFailed::Split)?;
    match result {
        None => Ok(Opened::Returned),
        Some(path) => Ok(Opened::Waited(wait_for(&host, &path, &pane))),
    }
}

fn margin_dir() -> PathBuf {
    crate::progress::default_path().with_file_name("margin")
}

/// Where Margin keeps plans it's handed, rounds and results.
pub fn data_dir() -> PathBuf {
    margin_dir()
}

/// Keeps `text` as the next round of the plan `key` when it differs from the
/// last one, and says which round it is and what the round before said.
fn record_round(key: &str, text: &str) -> std::io::Result<(u32, Option<String>)> {
    let dir = margin_dir()
        .join("rounds")
        .join(format!("{:016x}", crate::plugin::fnv1a(key)));
    record_round_in(&dir, text)
}

fn record_round_in(dir: &Path, text: &str) -> std::io::Result<(u32, Option<String>)> {
    std::fs::create_dir_all(dir)?;
    let read = |round: u32| std::fs::read_to_string(dir.join(format!("{round}.md"))).ok();
    let last = std::fs::read_dir(dir)?
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            name.to_str()?.strip_suffix(".md")?.parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0);
    if last > 0 && read(last).as_deref() == Some(text) {
        return Ok((last, last.checked_sub(1).and_then(read)));
    }
    std::fs::write(dir.join(format!("{}.md", last + 1)), text)?;
    Ok((last + 1, read(last)))
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
pub fn wait_for(host: &Host, path: &Path, pane: &str) -> Outcome {
    let mut checked = Instant::now();
    loop {
        if let Some(outcome) = read_result(path) {
            let _ = std::fs::remove_file(path);
            return outcome;
        }
        if checked.elapsed() >= PANE_CHECK {
            checked = Instant::now();
            if !host.is_open(pane) {
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
                // The agent was told to end its turn and wait, so a silent
                // close would leave it waiting with nothing to go on.
                let notice = "The review was closed without a verdict. Ask the person how they'd like to go on.";
                let delivered = match pane {
                    Some(pane) => agent::deliver_to_pane(pane, notice),
                    None => agent::deliver(notice),
                };
                match delivered {
                    Ok(found) => eprintln!(
                        "leaf: the review was closed without a verdict; told {}.",
                        found.describe()
                    ),
                    Err(e) => eprintln!("leaf: {e}"),
                }
                return ExitCode::SUCCESS;
            };
            let delivered = match pane {
                Some(pane) => agent::deliver_to_pane(pane, text),
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
                title: None,
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
    fn a_new_round_is_kept_only_when_the_plan_changed() {
        let dir = std::env::temp_dir().join(format!("leaf-rounds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(record_round_in(&dir, "one").ok(), Some((1, None)));
        assert_eq!(record_round_in(&dir, "one").ok(), Some((1, None)));
        assert_eq!(
            record_round_in(&dir, "two").ok(),
            Some((2, Some("one".into())))
        );
        assert_eq!(
            record_round_in(&dir, "two").ok(),
            Some((2, Some("one".into())))
        );
        assert_eq!(
            record_round_in(&dir, "three").ok(),
            Some((3, Some("two".into())))
        );
        let _ = std::fs::remove_dir_all(dir);
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
