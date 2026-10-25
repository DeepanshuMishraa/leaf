mod agent;
mod article;
mod comment;
mod diff;
mod diffview;
mod doc;
mod find;
mod font;
mod github;
mod highlight;
mod kitty;
mod markdown;
mod memory;
mod mermaid;
mod mono;
mod pdf;
mod progress;
mod render;
mod syntax;
mod viewer;

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use doc::Doc;
use progress::{Progress, Store};
use viewer::ViewerOptions;

const USAGE: &str = "Usage: leaf open <file.pdf | file.epub | file.md | url>
       leaf diff [options] [path | revision | GitHub url]

Diff options:
  --staged           staged changes
  --branch           this branch against its base branch
  --base <ref>       this branch against <ref>
  --range <spec>     any revision or range, such as main...HEAD
  --patch <file>     a .diff or .patch file

Diff keys: j/k move, v select, c comment, C comments, y copy, o other side,
r reload, ? keys, q quit

Reader keys: ←/→ page, j/k scroll, d/u half, space/b screen, g/G ends, t go to page, m bookmark, ' bookmarks, f find (n/N next/previous), i painted images, drag select + copy, y highlight, ? keys, q quit";

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
    match store.borrow().load_image_mode() {
        Ok(mode) => options.image_mode = mode.unwrap_or(options.image_mode),
        Err(e) => record(Err(e)),
    }
    match store.borrow().load_highlights(&key) {
        Ok(highlights) => options.highlights = highlights,
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
    options.on_highlights = {
        let (store, record, key) = (store.clone(), record.clone(), key.clone());
        Box::new(move |highlights| record(store.borrow_mut().save_highlights(&key, highlights)))
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
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match command.as_str() {
        "open" => {
            let [target] = rest else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            open(target)
        }
        "diff" => {
            if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            diff_command(rest)
        }
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
