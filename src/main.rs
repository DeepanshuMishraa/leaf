mod agent;
mod aloud;
mod article;
mod comment;
mod diff;
mod diffview;
mod doc;
mod edge;
mod find;
mod font;
mod gec;
mod github;
mod highlight;
mod hue;
mod kitty;
mod markdown;
mod memory;
mod mermaid;
mod mono;
mod pdf;
mod picker;
mod plugin;
mod progress;
mod remote;
mod render;
mod shape;
mod stroke;
mod syntax;
mod tempo;
mod viewer;

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use doc::Doc;
use progress::{Progress, Store};
use viewer::ViewerOptions;

const USAGE: &str = "Usage: leaf open [file.pdf | file.epub | file.md | url]   (nothing named: pick one)
       leaf diff [options] [path | revision | GitHub url]
       leaf comments [--json]
       leaf ask --title <name> [--question <text>]   (selection on stdin)
       leaf rex install | uninstall   add leaf to Rex's command palette
       leaf voice [name]        show or set the voice that reads aloud

Diff options:
  --staged           staged changes
  --branch           this branch against its base branch
  --base <ref>       this branch against <ref>
  --range <spec>     any revision or range, such as main...HEAD
  --patch <file>     a .diff or .patch file

Diff keys: j/k move, v visual, C comment, m comments, f files, y copy, o other
side, r reload, ? keys, q quit

Comments: leaf comments lists every saved review comment with its diff key.
  --json prints them for tools and agents.

Reader keys: ←/→ page, j/k scroll, ctrl+d/u half, space/b screen, g/G ends, t go to page, m bookmark, ' bookmarks, f find (n/N next/previous), i painted images, drag select + copy, @ or ctrl+enter send the selection, with the lines around it, to an agent, y highlight, 1-6 color, d pen, e eraser, v move ink, ! guide, ? keys, q quit

Rex: leaf rex install puts Leaf in the command palette: open (with a picker), review diffs, ask the agent pane about a selection, read aloud and pick a voice. In a remote session, leaf is installed on the host with cargo the first time.";

fn is_url(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn load(target: &str) -> Result<Doc, String> {
    if is_url(target) {
        return article::open(target).map_err(|e| e.to_string());
    }
    let lower = target.to_ascii_lowercase();
    if lower.ends_with(".epub") {
        // mupdf lays out the whole book to count its pages, which takes seconds for a long one.
        let terminal = std::io::stderr().is_terminal();
        if terminal {
            eprint!("Laying out {target}…");
        }
        let doc = pdf::open(Path::new(target)).map_err(|e| e.to_string());
        if terminal {
            eprint!("\r\x1b[K");
        }
        return doc;
    }
    if lower.ends_with(".pdf") {
        return pdf::open(Path::new(target)).map_err(|e| e.to_string());
    }
    if lower.ends_with(".md") || lower.ends_with(".markdown") {
        return markdown::open(Path::new(target)).map_err(|e| e.to_string());
    }
    Err(format!(
        "Can't open \"{target}\". leaf reads PDF files (.pdf), EPUB books (.epub), markdown files (.md) and web pages (http:// or https:// URLs)."
    ))
}

/// Remembers the image mode, each document's highlights and bookmarks, and where each
/// document (file or link) was left, so they come back next time. Progress that can't
/// be read or saved is reported after the reader closes; reading goes on.
fn resume(
    target: &str,
    page_count: Option<usize>,
    failure: &Rc<RefCell<Option<String>>>,
) -> ViewerOptions {
    let store = match Store::open(&progress::default_path()) {
        Ok(store) => Rc::new(RefCell::new(store)),
        Err(e) => {
            eprintln!("leaf: {e}");
            return ViewerOptions::default();
        }
    };
    let record = {
        let failure = failure.clone();
        move |result: Result<(), progress::StoreFailed>| {
            if let Err(e) = result {
                *failure.borrow_mut() = Some(e.to_string());
            }
        }
    };
    let key = if is_url(target) {
        target.to_owned()
    } else {
        std::fs::canonicalize(target)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| target.to_owned())
    };

    let mut options = ViewerOptions::default();
    options.source = Some(key.clone());
    match store.borrow().load_image_mode() {
        Ok(mode) => options.image_mode = mode.unwrap_or(options.image_mode),
        Err(e) => record(Err(e)),
    }
    match store.borrow().load_voice() {
        Ok(voice) => options.voice = voice.unwrap_or(options.voice),
        Err(e) => record(Err(e)),
    }
    match store.borrow().load_speed() {
        Ok(speed) => options.speed = speed.unwrap_or(options.speed),
        Err(e) => record(Err(e)),
    }
    match store.borrow().load_highlights(&key) {
        Ok(highlights) => options.highlights = highlights,
        Err(e) => record(Err(e)),
    }
    match store.borrow().load_strokes(&key) {
        // Like bookmarks, ink past the end was drawn on a longer copy of the file.
        Ok(strokes) => {
            options.strokes = strokes
                .into_iter()
                .filter(|s| page_count.is_none_or(|count| s.page < count))
                .collect()
        }
        Err(e) => record(Err(e)),
    }
    if let Some(count) = page_count {
        // A bookmark past the end was made in a longer copy of the file; it points nowhere now.
        match store.borrow().load_bookmarks(&key) {
            Ok(bookmarks) => {
                options.bookmarks = bookmarks.into_iter().filter(|b| b.page < count).collect()
            }
            Err(e) => record(Err(e)),
        }
    }
    if page_count.is_some() {
        match store.borrow().load(&key) {
            Ok(saved) => options.start_page = saved.map(|p| p.page),
            Err(e) => record(Err(e)),
        }
    } else {
        match store.borrow().load_scroll(&key) {
            Ok(saved) => options.start_anchor = saved.map(|s| s.anchor),
            Err(e) => record(Err(e)),
        }
    }

    options.on_image_mode = {
        let (store, record) = (store.clone(), record.clone());
        Box::new(move |mode| record(store.borrow().save_image_mode(mode)))
    };
    options.on_voice = {
        let (store, record) = (store.clone(), record.clone());
        Box::new(move |voice| record(store.borrow().save_voice(voice)))
    };
    options.on_speed = {
        let (store, record) = (store.clone(), record.clone());
        Box::new(move |speed| record(store.borrow().save_speed(speed)))
    };
    options.on_highlights = {
        let (store, record, key) = (store.clone(), record.clone(), key.clone());
        Box::new(move |highlights| record(store.borrow_mut().save_highlights(&key, highlights)))
    };
    options.on_strokes = {
        let (store, record, key) = (store.clone(), record.clone(), key.clone());
        Box::new(move |strokes| record(store.borrow_mut().save_strokes(&key, strokes)))
    };
    options.on_bookmarks = {
        let (store, record, key) = (store.clone(), record.clone(), key.clone());
        Box::new(move |bookmarks| record(store.borrow_mut().save_bookmarks(&key, bookmarks)))
    };
    match page_count {
        Some(count) => {
            options.on_page =
                Box::new(move |page| record(store.borrow().save(&key, Progress::of(page, count))))
        }
        None => {
            options.on_scroll =
                Box::new(move |scroll| record(store.borrow().save_scroll(&key, scroll)))
        }
    }
    options
}

fn main() -> ExitCode {
    memory::without_large_cache();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        if std::io::stdout().is_terminal() {
            return pick_and_open();
        }
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match command.as_str() {
        "open" => match rest {
            [] => pick_and_open(),
            [target] => open(target),
            _ => {
                eprintln!("{USAGE}");
                ExitCode::from(2)
            }
        },
        "diff" => {
            if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            diff_command(rest)
        }
        "comments" => {
            if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            comments_command(rest)
        }
        "ask" => {
            if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            ask_command(rest)
        }
        "rex" | "plugin" => plugin_command(rest),
        "voice" => voice_command(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Shows the document picker over this folder, then reads what was picked.
fn pick_and_open() -> ExitCode {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let recent = Store::open(&progress::default_path())
        .and_then(|store| store.recent(30))
        .unwrap_or_else(|e| {
            eprintln!("leaf: {e} The picker still lists files here.");
            Vec::new()
        });
    match picker::pick(&root, recent) {
        Ok(Some(target)) => open(&target),
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("leaf: the terminal failed ({e}). Name a document instead: leaf open FILE");
            ExitCode::from(1)
        }
    }
}

fn open(target: &str) -> ExitCode {
    std::thread::spawn(syntax::warm);
    let doc = match load(target) {
        Ok(doc) => doc,
        Err(message) => {
            eprintln!("leaf: {message}");
            return ExitCode::from(1);
        }
    };
    let failure = Rc::new(RefCell::new(None));
    let options = resume(target, doc.page_count, &failure);
    let result = viewer::run(doc, options);
    if let Some(message) = failure.borrow().as_ref() {
        eprintln!("leaf: {message}");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "leaf: the terminal failed ({e}). Your place and highlights up to now are saved."
            );
            ExitCode::from(1)
        }
    }
}

fn diff_command(args: &[String]) -> ExitCode {
    let request = match diff_request(args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("leaf: {message}");
            return ExitCode::from(2);
        }
    };
    std::thread::spawn(syntax::warm);
    let loaded = match request.load() {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    let failure = Rc::new(RefCell::new(None));
    let options = diff_options(&loaded.key, failure.clone());
    let result = diffview::run(loaded, options);
    if let Some(message) = failure.borrow().as_ref() {
        eprintln!("leaf: {message}");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "leaf: the terminal failed ({e}). Comments written up to now are saved when the store is writable."
            );
            ExitCode::from(1)
        }
    }
}

/// Pastes a selected passage, and an optional question, into an open agent pane.
fn ask_command(args: &[String]) -> ExitCode {
    let mut title = "this document".to_owned();
    let mut question = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--title" => {
                i += 1;
                title = args.get(i).cloned().unwrap_or_default();
            }
            "--question" => {
                i += 1;
                question = args.get(i).cloned().unwrap_or_default();
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("leaf: unknown option {other}. The selection is read from stdin.");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let mut passage = String::new();
    if !std::io::stdin().is_terminal() {
        use std::io::Read;
        if std::io::stdin().read_to_string(&mut passage).is_err() {
            eprintln!("leaf: could not read the selection.");
            return ExitCode::from(1);
        }
    }
    if passage.trim().is_empty() {
        eprintln!("leaf: select some text in the document first.");
        return ExitCode::from(2);
    }
    let context = agent::recall(&passage).unwrap_or_else(|| {
        agent::prompt(
            &title,
            None,
            &[agent::Excerpt {
                page: None,
                text: passage.clone(),
                start: 0,
                end: passage.chars().count(),
            }],
            "",
        )
    });
    let prompt = agent::with_question(&context, &question);
    match agent::deliver(&prompt) {
        Ok(found) => {
            println!("Sent to {}.", found.command);
            println!("LEAF_AGENT_BLOCK={}", found.pane);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("leaf: {e}");
            ExitCode::from(1)
        }
    }
}

/// Shows the voice that reads aloud, or makes `name` the one from now on.
fn voice_command(args: &[String]) -> ExitCode {
    let store = match Store::open(&progress::default_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    match args {
        [] => {
            let voice = store
                .load_voice()
                .ok()
                .flatten()
                .unwrap_or_else(|| edge::DEFAULT_VOICE.to_owned());
            println!("{voice}");
            ExitCode::SUCCESS
        }
        [name] if remote::is_voice_name(name) => match store.save_voice(name) {
            Ok(()) => {
                println!("leaf reads in {name} from now on.");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("leaf: {e}");
                ExitCode::from(1)
            }
        },
        _ => {
            eprintln!(
                "leaf: name one voice, such as en-GB-SoniaNeural. In a document, s searches every voice."
            );
            ExitCode::from(2)
        }
    }
}

fn plugin_command(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("install") => match plugin::install() {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("leaf: {e}");
                ExitCode::from(1)
            }
        },
        Some("uninstall") => match plugin::uninstall() {
            Ok(message) => {
                println!("{message}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("leaf: {e}");
                ExitCode::from(1)
            }
        },
        Some("-h" | "--help") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("leaf: unknown command {other}. Use leaf rex install or leaf rex uninstall.");
            ExitCode::from(2)
        }
    }
}

/// Lists saved review comments so an editor, script or agent can pick them up.
fn comments_command(args: &[String]) -> ExitCode {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other => {
                eprintln!("leaf: unknown option {other}. Run `leaf comments --help` for usage.");
                return ExitCode::from(2);
            }
        }
    }
    let store = match Store::open(&progress::default_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    let comments = match store.load_all_diff_comments() {
        Ok(comments) => comments,
        Err(e) => {
            eprintln!("leaf: {e}");
            return ExitCode::from(1);
        }
    };
    if json {
        print!("{}", comments_json(&comments));
    } else if comments.is_empty() {
        println!("No comments.");
    } else {
        for keyed in &comments {
            let comment = &keyed.saved.comment;
            println!(
                "{}  {}  {}",
                comment.label(),
                comment.anchor.side.as_str(),
                comment.state.label()
            );
            println!("  {}", keyed.key);
            for line in comment.body.lines() {
                println!("  {line}");
            }
            println!();
        }
    }
    ExitCode::SUCCESS
}

fn comments_json(comments: &[progress::KeyedComment]) -> String {
    use std::fmt::Write as _;

    if comments.is_empty() {
        return "[]\n".to_owned();
    }
    let mut out = String::from("[\n");
    for (index, keyed) in comments.iter().enumerate() {
        let comment = &keyed.saved.comment;
        let _ = writeln!(out, "  {{");
        let _ = writeln!(out, "    \"key\": \"{}\",", github::json_escape(&keyed.key));
        let _ = writeln!(out, "    \"id\": {},", keyed.saved.id);
        let _ = writeln!(
            out,
            "    \"file\": \"{}\",",
            github::json_escape(&comment.anchor.file)
        );
        let _ = writeln!(out, "    \"side\": \"{}\",", comment.anchor.side.as_str());
        let _ = writeln!(out, "    \"start_line\": {},", comment.anchor.start_line);
        let _ = writeln!(out, "    \"end_line\": {},", comment.anchor.end_line);
        let _ = writeln!(out, "    \"state\": \"{}\",", comment.state.as_str());
        let _ = writeln!(
            out,
            "    \"body\": \"{}\"",
            github::json_escape(&comment.body)
        );
        let comma = if index + 1 == comments.len() { "" } else { "," };
        let _ = writeln!(out, "  }}{comma}");
    }
    out.push_str("]\n");
    out
}

/// Opens the comment store and the comments saved for this diff. A store that
/// can't be opened only means comments last for this run.
fn diff_options(key: &str, failure: Rc<RefCell<Option<String>>>) -> diffview::Options {
    let mut options = diffview::Options {
        comments: Vec::new(),
        store: None,
        failure: failure.clone(),
    };
    match Store::open(&progress::default_path()) {
        Ok(store) => {
            match store.load_diff_comments(key) {
                Ok(comments) => options.comments = comments,
                Err(e) => *failure.borrow_mut() = Some(e.to_string()),
            }
            options.store = Some(store);
        }
        Err(e) => *failure.borrow_mut() = Some(e.to_string()),
    }
    options
}

fn is_patch(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("diff") || extension.eq_ignore_ascii_case("patch")
        })
}

fn looks_like_range(spec: &str) -> bool {
    spec.contains("..") || spec.contains('~') || spec.contains('^') || spec == "@"
}

fn set_origin(
    slot: &mut Option<diff::Origin>,
    origin: diff::Origin,
    what: &str,
) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!(
            "{what} can't be combined with another diff source. Use one of a path, a range, a patch file or a GitHub link."
        ));
    }
    *slot = Some(origin);
    Ok(())
}

/// Parses `leaf diff` arguments into a request, discovering the repository and
/// making paths relative to its root.
fn diff_request(args: &[String]) -> Result<diff::Request, String> {
    let mut origin: Option<diff::Origin> = None;
    let mut base: Option<String> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut options = true;
    let mut at = 0;
    while at < args.len() {
        let arg = &args[at];
        at += 1;
        if options && arg == "--" {
            options = false;
            continue;
        }
        if options && arg.starts_with('-') {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag, Some(value.to_owned())),
                None => (arg.as_str(), None),
            };
            let value = |at: &mut usize| -> Result<String, String> {
                if let Some(value) = inline.clone() {
                    return Ok(value);
                }
                let value = args.get(*at).cloned().ok_or_else(|| {
                    format!("{flag} needs a value, such as `{flag} main`. See `leaf diff --help`.")
                })?;
                *at += 1;
                Ok(value)
            };
            match flag {
                "--staged" | "-s" => {
                    set_origin(&mut origin, diff::Origin::Staged, "--staged")?;
                }
                "--branch" | "-b" => {
                    set_origin(&mut origin, diff::Origin::Branch { base: None }, "--branch")?;
                }
                "--base" => base = Some(value(&mut at)?),
                "--range" | "-r" => {
                    let spec = value(&mut at)?;
                    set_origin(&mut origin, diff::Origin::Range { spec }, "--range")?;
                }
                "--patch" => {
                    let path = value(&mut at)?;
                    set_origin(
                        &mut origin,
                        diff::Origin::Patch {
                            path: PathBuf::from(path),
                        },
                        "--patch",
                    )?;
                }
                other => {
                    return Err(format!(
                        "Unknown option {other}. Run `leaf diff --help` for the options."
                    ));
                }
            }
            continue;
        }
        let lower = arg.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            if github::Resource::parse_url(arg).is_none() {
                return Err(format!(
                    "That link isn't a GitHub pull request, commit or comparison: {arg}. Use a link like https://github.com/owner/repo/pull/12."
                ));
            }
            set_origin(&mut origin, diff::Origin::Github { url: arg.clone() }, arg)?;
            continue;
        }
        let path = Path::new(arg);
        if path.is_file() && is_patch(path) {
            set_origin(
                &mut origin,
                diff::Origin::Patch {
                    path: path.to_owned(),
                },
                arg,
            )?;
            continue;
        }
        if looks_like_range(arg) {
            set_origin(&mut origin, diff::Origin::Range { spec: arg.clone() }, arg)?;
            continue;
        }
        paths.push(arg.clone());
    }

    if let Some(base) = base {
        match origin {
            None => origin = Some(diff::Origin::Branch { base: Some(base) }),
            Some(diff::Origin::Branch { base: None }) => {
                origin = Some(diff::Origin::Branch { base: Some(base) })
            }
            Some(_) => {
                return Err(format!(
                    "--base {base} compares a branch with a base, so it can't be combined with a staged, ranged, patch or GitHub diff."
                ));
            }
        }
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let discover = |start: &Path| diff::Repo::discover(start).ok();
    let repo = paths
        .first()
        .map(Path::new)
        .and_then(discover)
        .or_else(|| discover(&cwd));

    // A name that isn't on disk may still be a branch or commit.
    let mut files = Vec::new();
    for path in paths {
        if Path::new(&path).exists() {
            let repo = repo.as_ref().ok_or_else(|| {
                diff::DiffFailed::NotRepo {
                    path: Path::new(&path).to_owned(),
                }
                .to_string()
            })?;
            let canonical = std::fs::canonicalize(&path).map_err(|e| {
                format!(
                    "Could not read {path} ({e}). Check that it exists and is readable, then try again."
                )
            })?;
            let inside = canonical.strip_prefix(&repo.root).map_err(|_| {
                format!(
                    "{path} is outside the repository at {}. Run leaf diff from inside the repository, or point it at a path in the same repository.",
                    repo.root.display()
                )
            })?;
            let relative = inside.to_string_lossy().replace('\\', "/");
            if !relative.is_empty() {
                files.push(relative);
            }
            continue;
        }
        if let Some(repo) = &repo
            && repo.has_rev(&path)
        {
            set_origin(
                &mut origin,
                diff::Origin::Range { spec: path.clone() },
                &path,
            )?;
            continue;
        }
        return Err(format!(
            "{path} is not a file, a directory, a revision in this repository, or a GitHub link. Check the path, or pass a revision with `--range {path}`."
        ));
    }

    let origin = origin.unwrap_or(diff::Origin::Working);
    let local = matches!(
        origin,
        diff::Origin::Working
            | diff::Origin::Staged
            | diff::Origin::Branch { .. }
            | diff::Origin::Range { .. }
    );
    if local && repo.is_none() {
        return Err(diff::DiffFailed::NotRepo { path: cwd }.to_string());
    }
    Ok(diff::Request {
        origin,
        repo,
        paths: files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::{Anchor, Comment, Side, State};
    use crate::progress::{KeyedComment, SavedComment};

    fn keyed(body: &str) -> KeyedComment {
        KeyedComment {
            key: "diff:git:/repo:working".to_owned(),
            saved: SavedComment {
                id: 7,
                comment: Comment {
                    anchor: Anchor {
                        file: "src/main.rs".to_owned(),
                        side: Side::New,
                        start_line: 11,
                        end_line: 12,
                    },
                    body: body.to_owned(),
                    state: State::Draft,
                },
            },
        }
    }

    #[test]
    fn comments_json_carries_the_key_and_anchor() {
        let json = comments_json(&[keyed("why?\nreally")]);
        assert!(json.starts_with("[\n  {\n"));
        assert!(json.contains("\"key\": \"diff:git:/repo:working\""));
        assert!(json.contains("\"id\": 7"));
        assert!(json.contains("\"file\": \"src/main.rs\""));
        assert!(json.contains("\"side\": \"new\""));
        assert!(json.contains("\"start_line\": 11"));
        assert!(json.contains("\"end_line\": 12"));
        assert!(json.contains("\"state\": \"draft\""));
        assert!(json.contains("\"body\": \"why?\\nreally\""));
        assert!(json.ends_with("\n  }\n]\n"));
    }

    #[test]
    fn no_comments_is_an_empty_json_list() {
        assert_eq!(comments_json(&[]), "[]\n");
    }
}
