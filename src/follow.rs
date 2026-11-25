//! Where a clicked link leads: a web page for the browser, or a file for
//! leaf to open in place of the one being read.

use std::path::{Path, PathBuf};

use crate::render::Link;

/// What following a link does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Follow {
    /// Opens in the system's browser (or mail app, for `mailto:`).
    Web(String),
    /// Opens in leaf.
    Open(PathBuf),
}

/// How many entries a vault search looks at before giving up.
const SEARCH_LIMIT: usize = 20_000;

/// Where `link` leads from the document at `doc` (`None` for one read from a URL).
/// The error says why it leads nowhere, for the footer.
pub fn resolve(link: &Link, doc: Option<&Path>) -> Result<Follow, String> {
    // `leaf notes.md` passes a path relative to the working directory, whose
    // folder is empty: links are followed from where the file really is.
    let doc = doc.and_then(|doc| std::path::absolute(doc).ok());
    let dir = doc.as_deref().and_then(Path::parent);
    match link {
        Link::Url(url) => url_target(url, dir),
        Link::Note(target) => {
            let dir = dir.ok_or("wikilinks only lead somewhere in a file on disk")?;
            note(target, dir).map(Follow::Open)
        }
    }
}

fn url_target(url: &str, dir: Option<&Path>) -> Result<Follow, String> {
    let url = url.trim();
    if url.is_empty() || url.starts_with('#') {
        return Err("that link points within this page".into());
    }
    let lower = url.to_ascii_lowercase();
    if ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return Ok(Follow::Web(url.to_owned()));
    }
    if has_scheme(url) {
        return Err(format!(
            "leaf only follows web, mail and file links, not {url}"
        ));
    }
    let dir = dir.ok_or("a relative link only leads somewhere in a file on disk")?;
    let path = relative(url, dir).ok_or_else(|| format!("couldn't read the link {url}"))?;
    if path.is_file() {
        Ok(Follow::Open(path))
    } else {
        Err(format!("there's no file at {}", path.display()))
    }
}

/// Whether `url` starts with a scheme such as `ftp:` or `javascript:`.
fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// A relative link from a file in `dir`, decoded, without its `#fragment` or `?query`.
fn relative(link: &str, dir: &Path) -> Option<PathBuf> {
    let base = url::Url::from_directory_path(dir).ok()?;
    base.join(link).ok()?.to_file_path().ok()
}

/// The note a wikilink names: beside the file first, then anywhere in its
/// Obsidian vault (the nearest folder above holding `.obsidian`), or below
/// its folder when it isn't in one. The nearest match wins.
fn note(target: &str, dir: &Path) -> Result<PathBuf, String> {
    // `[[note#heading]]` and `[[note#^block]]` lead to the note.
    let name = target.split('#').next().unwrap_or_default().trim();
    if name.is_empty() {
        return Err("that wikilink points within this note".into());
    }
    let wanted = with_extension(name);
    for candidate in [dir.join(name), dir.join(&wanted)] {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let root = vault(dir).unwrap_or(dir);
    let suffix: Vec<&str> = wanted.split('/').filter(|part| !part.is_empty()).collect();
    let mut found: Option<PathBuf> = None;
    let mut seen = 0;
    search(root, &suffix, &mut found, &mut seen);
    found.ok_or_else(|| format!("no note named {name} in {}", root.display()))
}

/// `name` as a file name: notes without an extension are markdown.
fn with_extension(name: &str) -> String {
    let file = name.rsplit('/').next().unwrap_or(name);
    if Path::new(file).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}.md")
    }
}

fn vault(dir: &Path) -> Option<&Path> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".obsidian").is_dir())
}

/// Looks under `dir` for a file whose path ends with `suffix`, keeping the
/// one fewest folders down.
fn search(dir: &Path, suffix: &[&str], found: &mut Option<PathBuf>, seen: &mut usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut folders = Vec::new();
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > SEARCH_LIMIT {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            folders.push(path);
        } else if ends_with(&path, suffix) {
            let depth = |p: &Path| p.components().count();
            if found
                .as_deref()
                .is_none_or(|best| depth(&path) < depth(best))
            {
                *found = Some(path);
            }
        }
    }
    folders.sort();
    for folder in folders {
        search(&folder, suffix, found, seen);
    }
}

fn ends_with(path: &Path, suffix: &[&str]) -> bool {
    let parts: Vec<_> = path.components().rev().take(suffix.len()).collect();
    parts.len() == suffix.len()
        && parts
            .iter()
            .zip(suffix.iter().rev())
            .all(|(part, want)| part.as_os_str() == *want)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-follow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("folders");
        std::fs::write(path, "# note").expect("file");
    }

    fn url(link: &str) -> Link {
        Link::Url(link.into())
    }

    fn wiki(target: &str) -> Link {
        Link::Note(target.into())
    }

    #[test]
    fn web_and_mail_links_go_to_the_browser() {
        let doc = Path::new("/notes/today.md");
        for link in ["https://x.dev/a?b#c", "HTTP://x.dev", "mailto:me@x.dev"] {
            assert_eq!(resolve(&url(link), Some(doc)), Ok(Follow::Web(link.into())));
        }
        // An article read from the web still opens its links.
        assert_eq!(
            resolve(&url("https://x.dev"), None),
            Ok(Follow::Web("https://x.dev".into()))
        );
    }

    #[test]
    fn other_schemes_and_anchors_lead_nowhere() {
        let doc = Path::new("/notes/today.md");
        for link in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "ftp://x",
            "#top",
            "",
        ] {
            assert!(resolve(&url(link), Some(doc)).is_err(), "{link}");
        }
    }

    #[test]
    fn links_resolve_when_the_file_was_opened_by_a_relative_path() {
        // As `leaf notes.md` would pass it: relative to the working directory.
        let folder = format!(".leaf-follow-{}-cwd", std::process::id());
        let cwd = std::env::current_dir().expect("working directory");
        let _ = std::fs::remove_dir_all(cwd.join(&folder));
        touch(&cwd.join(&folder).join("today.md"));
        touch(&cwd.join(&folder).join("plan.md"));
        let doc = PathBuf::from(&folder).join("today.md");
        let plan = cwd.join(&folder).join("plan.md");
        assert_eq!(
            resolve(&url("plan.md"), Some(&doc)),
            Ok(Follow::Open(plan.clone()))
        );
        assert_eq!(resolve(&wiki("plan"), Some(&doc)), Ok(Follow::Open(plan)));
        std::fs::remove_dir_all(cwd.join(&folder)).expect("clean up");
    }

    #[test]
    fn a_relative_link_opens_the_file_it_names() {
        let dir = temp("relative");
        let doc = dir.join("notes/today.md");
        touch(&doc);
        touch(&dir.join("notes/my plan.md"));
        touch(&dir.join("readme.md"));
        assert_eq!(
            resolve(&url("my%20plan.md#steps"), Some(&doc)),
            Ok(Follow::Open(dir.join("notes/my plan.md")))
        );
        assert_eq!(
            resolve(&url("../readme.md"), Some(&doc)),
            Ok(Follow::Open(dir.join("readme.md")))
        );
        assert!(resolve(&url("missing.md"), Some(&doc)).is_err());
        assert!(resolve(&url("readme.md"), None).is_err());
    }

    #[test]
    fn a_wikilink_finds_its_note_beside_the_file_then_in_the_vault() {
        let vault = temp("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).expect("vault marker");
        let doc = vault.join("daily/today.md");
        touch(&doc);
        touch(&vault.join("daily/standup.md"));
        touch(&vault.join("projects/plan.md"));
        touch(&vault.join("archive/old/plan.md"));
        touch(&vault.join("pics/diagram.png"));
        let open = |target: &str| resolve(&wiki(target), Some(&doc));
        assert_eq!(
            open("standup"),
            Ok(Follow::Open(vault.join("daily/standup.md")))
        );
        // The nearest of two notes with the same name.
        assert_eq!(
            open("plan#Steps"),
            Ok(Follow::Open(vault.join("projects/plan.md")))
        );
        assert_eq!(
            open("old/plan"),
            Ok(Follow::Open(vault.join("archive/old/plan.md")))
        );
        assert_eq!(
            open("diagram.png"),
            Ok(Follow::Open(vault.join("pics/diagram.png")))
        );
        assert!(open("nowhere").is_err());
        assert!(open("#heading").is_err());
        assert!(resolve(&wiki("standup"), None).is_err());
    }
}
