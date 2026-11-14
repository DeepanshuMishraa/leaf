//! The skills leaf ships for coding agents: what leaf is and how to drive it,
//! and how to hand plans and changes to the person for review. Installed
//! copies carry a stamp of what was written, so leaf keeps its own copies
//! current and never touches one the person edited.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::plugin::fnv1a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skill {
    pub name: &'static str,
    text: &'static str,
}

pub const SKILLS: [Skill; 3] = [
    Skill {
        name: "leaf",
        text: include_str!("../skills/leaf/SKILL.md"),
    },
    Skill {
        name: "leaf-plan",
        text: include_str!("../skills/leaf-plan/SKILL.md"),
    },
    Skill {
        name: "leaf-review",
        text: include_str!("../skills/leaf-review/SKILL.md"),
    },
];

/// The front matter key that stamps a file as written by leaf.
const STAMP: &str = "  leaf-skill: ";

impl Skill {
    fn version(self) -> u64 {
        fnv1a(self.text)
    }

    /// The file as installed: the skill with leaf's stamp in its front matter.
    fn rendered(self) -> String {
        stamped(self.text, self.version())
    }
}

/// `text` with a stamp of `version` added to its front matter.
fn stamped(text: &str, version: u64) -> String {
    let stamp = format!("metadata:\n{STAMP}\"{version:016x}\"\n");
    match text
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n"))
    {
        Some(end) => {
            let close = 4 + end + 1;
            format!("{}{stamp}{}", &text[..close], &text[close..])
        }
        None => format!("---\n{stamp}---\n\n{text}"),
    }
}

/// `text` without leaf's stamp, and the version the stamp names.
fn unstamped(text: &str) -> Option<(String, u64)> {
    let start = text.find("metadata:\n")?;
    let stamp_at = start + "metadata:\n".len();
    let line_end = stamp_at + text[stamp_at..].find('\n')?;
    let value = text[stamp_at..line_end].strip_prefix(STAMP)?;
    let version = u64::from_str_radix(value.trim_matches('"'), 16).ok()?;
    let body = format!("{}{}", &text[..start], &text[line_end + 1..]);
    Some((body, version))
}

/// What's at a skill's path now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    Missing,
    /// Written by leaf and unchanged since; `current` when it's this version.
    Ours {
        current: bool,
    },
    /// Written by leaf, then edited by hand: left alone.
    Edited,
    /// Not written by leaf: left alone.
    Foreign,
}

fn path_of(dir: &Path, skill: Skill) -> PathBuf {
    dir.join(skill.name).join("SKILL.md")
}

pub fn found(dir: &Path, skill: Skill) -> Found {
    let text = match fs::read_to_string(path_of(dir, skill)) {
        Ok(text) => text,
        // Only a file that isn't there counts as leaf's to write. One that
        // can't be read (bad UTF-8, permissions) is treated as not leaf's,
        // so installing leaves it alone.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Found::Missing,
        Err(_) => return Found::Foreign,
    };
    match unstamped(&text) {
        None => Found::Foreign,
        Some((body, version)) if fnv1a(&body) == version => Found::Ours {
            current: version == skill.version(),
        },
        Some(_) => Found::Edited,
    }
}

/// What installing one skill in one folder did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub path: PathBuf,
    pub result: Result<Change, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Written,
    Updated,
    AlreadyCurrent,
    /// Edited by hand or not leaf's: left as it was.
    Kept,
    Removed,
    Absent,
}

impl Change {
    fn label(self) -> &'static str {
        match self {
            Change::Written => "installed",
            Change::Updated => "updated",
            Change::AlreadyCurrent => "already current",
            Change::Kept => "left alone (edited by hand, or not leaf's)",
            Change::Removed => "removed",
            Change::Absent => "not installed",
        }
    }
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Installs every skill into each folder of skills.
pub fn install(dirs: &[PathBuf]) -> Vec<Report> {
    let mut reports = Vec::new();
    for dir in dirs {
        for skill in SKILLS {
            let path = path_of(dir, skill);
            let result = match found(dir, skill) {
                Found::Missing => write(&path, &skill.rendered()).map(|()| Change::Written),
                Found::Ours { current: false } => {
                    write(&path, &skill.rendered()).map(|()| Change::Updated)
                }
                Found::Ours { current: true } => Ok(Change::AlreadyCurrent),
                Found::Edited | Found::Foreign => Ok(Change::Kept),
            };
            reports.push(Report { path, result });
        }
    }
    reports
}

/// Removes leaf's own, unedited skills from each folder.
pub fn uninstall(dirs: &[PathBuf]) -> Vec<Report> {
    let mut reports = Vec::new();
    for dir in dirs {
        for skill in SKILLS {
            let path = path_of(dir, skill);
            let result = match found(dir, skill) {
                Found::Missing => Ok(Change::Absent),
                Found::Ours { .. } => fs::remove_file(&path)
                    .map(|()| {
                        if let Some(parent) = path.parent() {
                            let _ = fs::remove_dir(parent);
                        }
                        Change::Removed
                    })
                    .map_err(|e| format!("{}: {e}", path.display())),
                Found::Edited | Found::Foreign => Ok(Change::Kept),
            };
            reports.push(Report { path, result });
        }
    }
    reports
}

/// Brings leaf's own installed skills up to this version. Quiet: a copy that
/// can't be updated keeps working as it was.
pub fn refresh() {
    for dir in all_dirs() {
        for skill in SKILLS {
            if found(&dir, skill) == (Found::Ours { current: false }) {
                let _ = write(&path_of(&dir, skill), &skill.rendered());
            }
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where agents look for skills: Claude Code's folder, and the shared one
/// Codex, OpenCode, Pi and others read.
pub fn default_dirs() -> Vec<PathBuf> {
    let home = home();
    vec![home.join(".claude/skills"), home.join(".agents/skills")]
}

/// Every folder a leaf skill may already live in, including the separate
/// ones Cursor and Antigravity read. Upgrades refresh these too.
fn all_dirs() -> Vec<PathBuf> {
    let home = home();
    let mut dirs = default_dirs();
    for extra in [".cursor/skills", ".gemini/config/skills"] {
        let dir = home.join(extra);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

const USAGE: &str = "Usage: leaf skills install | uninstall | status | print <name>

Skills teach coding agents to use leaf: `leaf` (reading, diffs, pull requests
and every key), `leaf-plan` (hand a plan over for review) and `leaf-review`
(hand changes over for review).

install    writes them to ~/.claude/skills and ~/.agents/skills
uninstall  removes the copies leaf wrote (ones you edited stay)
status     says what's installed where
print      writes one skill to stdout, for agents that take a file";

fn print_reports(reports: &[Report]) -> ExitCode {
    let mut failed = false;
    for report in reports {
        match &report.result {
            Ok(change) => println!("{}  {}", change.label(), report.path.display()),
            Err(e) => {
                failed = true;
                eprintln!("leaf: couldn't write {} ({e})", report.path.display());
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `leaf skills`.
pub fn command(args: &[String]) -> ExitCode {
    match args {
        [verb] if verb == "install" => print_reports(&install(&default_dirs())),
        [verb] if verb == "uninstall" => print_reports(&uninstall(&default_dirs())),
        [verb] if verb == "status" => {
            for dir in default_dirs() {
                for skill in SKILLS {
                    let state = match found(&dir, skill) {
                        Found::Missing => "not installed",
                        Found::Ours { current: true } => "installed, current",
                        Found::Ours { current: false } => "installed, out of date",
                        Found::Edited => "edited by hand",
                        Found::Foreign => "someone else's file",
                    };
                    println!("{state:<22}  {}", path_of(&dir, skill).display());
                }
            }
            ExitCode::SUCCESS
        }
        [verb, name] if verb == "print" => match SKILLS.iter().find(|s| s.name == name) {
            Some(skill) => {
                print!("{}", skill.text);
                ExitCode::SUCCESS
            }
            None => {
                let names: Vec<&str> = SKILLS.iter().map(|s| s.name).collect();
                eprintln!(
                    "leaf: there's no skill called {name}. The skills are {}.",
                    names.join(", ")
                );
                ExitCode::from(2)
            }
        },
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("leaf-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn every_skill_has_front_matter_naming_it() {
        for skill in SKILLS {
            assert!(
                skill
                    .text
                    .starts_with(&format!("---\nname: {}\n", skill.name)),
                "{}",
                skill.name
            );
            assert!(skill.text.contains("\ndescription: "));
        }
    }

    #[test]
    fn the_stamp_goes_in_the_front_matter_and_comes_out_again() {
        let skill = SKILLS[1];
        let rendered = skill.rendered();
        let head = &rendered[..rendered.find("\n---\n").unwrap_or(0)];
        assert!(head.contains("metadata:\n  leaf-skill: \""), "{rendered}");
        assert_eq!(
            unstamped(&rendered),
            Some((skill.text.to_owned(), skill.version()))
        );
        assert_eq!(unstamped(skill.text), None);
    }

    #[test]
    fn install_writes_updates_and_leaves_edited_files_alone() {
        let dir = scratch("install");
        let dirs = vec![dir.clone()];
        let first = install(&dirs);
        assert!(first.iter().all(|r| r.result == Ok(Change::Written)));
        assert!(
            install(&dirs)
                .iter()
                .all(|r| r.result == Ok(Change::AlreadyCurrent))
        );
        // An older version of leaf's own file is brought up to date.
        let plan = path_of(&dir, SKILLS[1]);
        let old = stamped(
            "---\nname: leaf-plan\n---\nold\n",
            fnv1a("---\nname: leaf-plan\n---\nold\n"),
        );
        fs::write(&plan, old).unwrap_or_default();
        assert_eq!(found(&dir, SKILLS[1]), Found::Ours { current: false });
        assert_eq!(install(&dirs)[1].result, Ok(Change::Updated));
        // A hand edit is kept, by install and uninstall alike.
        let edited = SKILLS[1].rendered().replace("End your turn", "Stop");
        fs::write(&plan, &edited).unwrap_or_default();
        assert_eq!(found(&dir, SKILLS[1]), Found::Edited);
        assert_eq!(install(&dirs)[1].result, Ok(Change::Kept));
        let removed = uninstall(&dirs);
        assert_eq!(removed[0].result, Ok(Change::Removed));
        assert_eq!(removed[1].result, Ok(Change::Kept));
        assert_eq!(fs::read_to_string(&plan).ok(), Some(edited));
        assert_eq!(found(&dir, SKILLS[0]), Found::Missing);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn someone_elses_skill_of_the_same_name_is_left_alone() {
        let dir = scratch("foreign");
        let path = path_of(&dir, SKILLS[0]);
        write(&path, "---\nname: leaf\n---\nmine\n").unwrap_or_default();
        assert_eq!(found(&dir, SKILLS[0]), Found::Foreign);
        assert_eq!(
            install(std::slice::from_ref(&dir))[0].result,
            Ok(Change::Kept)
        );
        assert_eq!(
            uninstall(std::slice::from_ref(&dir))[0].result,
            Ok(Change::Kept)
        );
        let _ = fs::remove_dir_all(dir);
    }
}
