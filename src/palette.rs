//! Lists for Rex's own command palette. Rex 17 and later can ask a question
//! in the palette from a Lua action (`rex.ui.pick`), so the plugin shows
//! leaf's pickers there instead of in a terminal popover. leaf prints the
//! rows as JSON; the plugin asks, then acts on the id that comes back.

use std::collections::HashSet;
use std::path::Path;

use serde::Serialize;

use crate::agent::Installed;
use crate::edge::Voice;
use crate::picker::{self, Entry};

/// Rex shows at most this many rows in one question.
const MOST: usize = 2000;
/// Recent documents listed above the folder.
const RECENT: usize = 8;

/// One row of a `rex.ui.pick` question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    /// One of Rex's icon names; any other is dropped with a warning.
    pub icon: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// What a path typed or picked in the document picker turned out to be.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Listing {
    /// A folder to browse. Ids are `dir:<path>` to go in, `open:<target>` to read.
    Folder {
        dir: String,
        title: String,
        items: Vec<Item>,
    },
    /// A file to read as it is.
    Document { target: String },
}

/// What `path` holds: its folders and documents, after the recent documents
/// when `recent` is given.
pub fn documents(path: &str, recent: Option<&[(String, usize)]>) -> Result<Listing, String> {
    let expanded = picker::expand(path.trim());
    let dir = std::fs::canonicalize(&expanded).map_err(|e| {
        format!(
            "{} isn't a folder or file leaf can see ({e}). Check the path and try again.",
            expanded.display()
        )
    })?;
    // Rex ids are text, so a path that isn't UTF-8 couldn't come back intact.
    let Some(shown_dir) = dir.to_str().map(str::to_owned) else {
        return Err(format!(
            "{} has a name that isn't UTF-8, which Rex's palette can't pass back. Open it with leaf open in a terminal.",
            dir.display()
        ));
    };
    if !dir.is_dir() {
        return Ok(Listing::Document { target: shown_dir });
    }
    let mut items = Vec::new();
    let mut listed = HashSet::new();
    for (target, percent) in recent.unwrap_or_default() {
        if items.len() == RECENT {
            break;
        }
        if !picker::is_link(target) && !Path::new(target).is_file() {
            continue;
        }
        listed.insert(target.clone());
        items.push(Item {
            id: format!("open:{target}"),
            title: target.rsplit('/').next().unwrap_or(target).to_owned(),
            subtitle: picker::tilde(target),
            icon: if picker::is_link(target) {
                "link"
            } else {
                "clock"
            },
            detail: format!("{percent}%"),
        });
    }
    if let Some(parent) = dir.parent().and_then(Path::to_str) {
        items.push(Item {
            id: format!("dir:{parent}"),
            title: "..".to_owned(),
            subtitle: picker::tilde(parent),
            icon: "folder",
            detail: String::new(),
        });
    }
    let room = MOST - items.len();
    items.extend(
        picker::children(&dir, "")
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Folder(path) => row("dir", &path, "folder"),
                Entry::Document(path)
                    if !path.to_str().is_some_and(|path| listed.contains(path)) =>
                {
                    row("open", &path, "document")
                }
                _ => None,
            })
            .take(room),
    );
    let shown = picker::tilde(&shown_dir);
    Ok(Listing::Folder {
        dir: shown_dir,
        title: format!("Open Document · {shown}"),
        items,
    })
}

/// A folder or document row; `None` for a path that isn't UTF-8, since the
/// id Rex sends back couldn't name it.
fn row(kind: &str, path: &Path, icon: &'static str) -> Option<Item> {
    let text = path.to_str()?;
    Some(Item {
        id: format!("{kind}:{text}"),
        title: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(text)
            .to_owned(),
        subtitle: String::new(),
        icon,
        detail: String::new(),
    })
}

/// The voices, the one reading now first. Ids are voice names.
pub fn voices(voices: &[Voice], current: &str) -> Vec<Item> {
    let mut items: Vec<Item> = voices
        .iter()
        .map(|voice| Item {
            id: voice.name.clone(),
            title: voice.short().to_owned(),
            subtitle: voice.language.clone(),
            icon: "person",
            detail: if voice.name == current {
                format!("{} · current", voice.gender)
            } else {
                voice.gender.clone()
            },
        })
        .take(MOST)
        .collect();
    if let Some(at) = items.iter().position(|item| item.id == current) {
        let item = items.remove(at);
        items.insert(0, item);
    }
    items
}

/// The id of the "Any agent pane" row, which clears the default.
pub const ANY_AGENT: &str = "any";

/// Any agent pane, then each installed agent. Ids are `any` or agent names.
pub fn agents(installed: &[Installed], current: Option<&str>) -> Vec<Item> {
    let mark = |picked: bool| if picked { "current" } else { "" }.to_owned();
    std::iter::once(Item {
        id: ANY_AGENT.to_owned(),
        title: "Any agent pane".to_owned(),
        subtitle: "The focused one first, else one in the same tab".to_owned(),
        icon: "terminal",
        detail: mark(current.is_none()),
    })
    .chain(installed.iter().map(|agent| {
        Item {
            id: agent.name.to_owned(),
            title: agent.name.to_owned(),
            subtitle: agent
                .path
                .parent()
                .map(|dir| picker::tilde(&dir.display().to_string()))
                .unwrap_or_default(),
            icon: "terminal",
            detail: mark(current == Some(agent.name)),
        }
    }))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-palette-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("books")).unwrap();
        std::fs::write(dir.join("notes.md"), "# A").unwrap();
        std::fs::write(dir.join("paper.pdf"), "").unwrap();
        std::fs::write(dir.join("build.rs"), "").unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn ids(listing: &Listing) -> Vec<String> {
        match listing {
            Listing::Folder { items, .. } => items.iter().map(|i| i.id.clone()).collect(),
            Listing::Document { target } => vec![target.clone()],
        }
    }

    #[test]
    fn a_folder_lists_recent_then_up_then_folders_and_documents_once() {
        let dir = tree("folder");
        let d = dir.display();
        let notes = format!("{d}/notes.md");
        let recent = vec![
            (notes.clone(), 40),
            ("/gone.pdf".to_owned(), 3),
            ("https://example.com/post".to_owned(), 10),
        ];
        let listing = documents(&d.to_string(), Some(&recent)).unwrap();
        assert_eq!(
            ids(&listing),
            [
                format!("open:{notes}"),
                "open:https://example.com/post".to_owned(),
                format!("dir:{}", dir.parent().unwrap().display()),
                format!("dir:{d}/books"),
                format!("open:{d}/paper.pdf"),
            ],
            "a gone file and other files are left out, and notes.md shows once"
        );
        let unique: HashSet<_> = ids(&listing).into_iter().collect();
        assert_eq!(unique.len(), ids(&listing).len(), "Rex needs unique ids");
        assert!(
            matches!(listing, Listing::Folder { ref title, .. } if title.starts_with("Open Document · "))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_is_a_document_and_a_missing_path_says_so() {
        let dir = tree("file");
        let notes = dir.join("notes.md").display().to_string();
        assert_eq!(
            documents(&notes, None),
            Ok(Listing::Document { target: notes })
        );
        let missing = documents(&dir.join("nope").display().to_string(), None).unwrap_err();
        assert!(missing.contains("isn't a folder or file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_listing_is_tagged_json_rex_can_decode() {
        let json = serde_json::to_value(Listing::Document {
            target: "/a.pdf".into(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "document", "target": "/a.pdf"})
        );
        let item = serde_json::to_value(row("dir", Path::new("/a/b"), "folder")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let odd = Path::new(std::ffi::OsStr::from_bytes(b"/a/b\xff.pdf"));
            assert_eq!(
                row("open", odd, "document"),
                None,
                "a lossy id couldn't open it"
            );
        }
        assert_eq!(
            item,
            serde_json::json!({"id": "dir:/a/b", "title": "b", "icon": "folder"})
        );
    }

    #[test]
    fn the_current_voice_comes_first_and_is_marked() {
        let all = crate::edge::built_in();
        let current = &all[3].name;
        let items = voices(&all, current);
        assert_eq!(items.len(), all.len());
        assert_eq!(&items[0].id, current);
        assert!(items[0].detail.ends_with("· current"));
        assert!(items[1..].iter().all(|i| !i.detail.contains("current")));
    }

    #[test]
    fn any_agent_pane_leads_and_the_picked_agent_is_marked() {
        let installed: Vec<Installed> = ["claude", "codex"]
            .into_iter()
            .map(|name| Installed {
                name,
                path: PathBuf::from(format!("/bin/{name}")),
            })
            .collect();
        let items = agents(&installed, Some("codex"));
        let ids: Vec<_> = items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, [ANY_AGENT, "claude", "codex"]);
        assert_eq!(items[2].detail, "current");
        assert!(agents(&installed, None)[0].detail == "current");
    }
}
