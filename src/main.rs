mod article;
mod doc;
mod find;
mod font;
mod highlight;
mod kitty;
mod memory;
mod mono;
mod pdf;
mod progress;
mod render;
mod viewer;

use std::cell::RefCell;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;

use doc::Doc;
use progress::{Progress, Store};
use viewer::ViewerOptions;

const USAGE: &str = "Usage: leaf open <file.pdf | url>

Keys: ←/→ page, j/k scroll, d/u half, space/b screen, g/G ends, t go to page, f find (n/N next/previous), i painted images, drag select + copy, y highlight, ? keys, q quit";

fn is_url(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

fn load(target: &str) -> Result<Doc, String> {
    if is_url(target) {
        return article::open(target).map_err(|e| e.to_string());
    }
    if target.to_ascii_lowercase().ends_with(".pdf") {
        return pdf::open(Path::new(target)).map_err(|e| e.to_string());
    }
    Err(format!("Can't open \"{target}\". leaf reads PDF files (.pdf) and web pages (http:// or https:// URLs)."))
}

/// Remembers the image mode, each document's highlights, and where each PDF
/// (file or link) was left, so they come back next time. Progress that can't
/// be read or saved is reported after the reader closes; reading goes on.
fn resume(target: &str, page_count: Option<usize>, failure: &Rc<RefCell<Option<String>>>) -> ViewerOptions {
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
        std::fs::canonicalize(target).map(|p| p.display().to_string()).unwrap_or_else(|_| target.to_owned())
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
    if page_count.is_some() {
        match store.borrow().load(&key) {
            Ok(saved) => options.start_page = saved.map(|p| p.page),
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
    if let Some(count) = page_count {
        options.on_page = Box::new(move |page| record(store.borrow().save(&key, Progress::of(page, count))));
    }
    options
}

fn main() -> ExitCode {
    memory::without_large_cache();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, target] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    if command != "open" {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
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
            eprintln!("leaf: the terminal failed ({e}). Your place and highlights up to now are saved.");
            ExitCode::from(1)
        }
    }
}
