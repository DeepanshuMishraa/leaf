//! `leaf agents`: pick the agent CLI that questions about a selection go to,
//! in a Rex-style list of the ones installed here.

use std::io;

use crossterm::event::{self, Event, KeyEventKind};

use crate::agent::Installed;
use crate::popover::{self, Edit, Row, Section};

/// What a row of the list sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// The focused agent pane, else one in the same tab: no default.
    Any,
    Agent(&'static str),
}

fn listing(
    installed: &[Installed],
    typed: &str,
    current: Option<&str>,
) -> (Vec<Section>, Vec<Pick>) {
    let typed = typed.trim().to_ascii_lowercase();
    let any_matches = typed.is_empty() || "any agent pane".contains(&typed);
    let mut picks = Vec::new();
    let mut any_rows = Vec::new();
    if any_matches {
        picks.push(Pick::Any);
        any_rows.push(Row {
            icon: "",
            label: "Any agent pane".into(),
            detail: "the focused one first  ".into(),
            checked: current.is_none(),
        });
    }
    let mut rows = Vec::new();
    for agent in installed.iter().filter(|a| a.name.contains(typed.as_str())) {
        picks.push(Pick::Agent(agent.name));
        rows.push(Row {
            icon: "",
            label: agent.name.to_owned(),
            detail: agent
                .path
                .parent()
                .map(|dir| format!("{}  ", dir.display()))
                .unwrap_or_default(),
            checked: current == Some(agent.name),
        });
    }
    let title = format!("{} installed", installed.len());
    (
        vec![
            Section {
                title: String::new(),
                rows: any_rows,
            },
            Section { title, rows },
        ],
        picks,
    )
}

/// Shows the installed agents until one is picked, or `None` when closed.
pub fn pick(installed: &[Installed], current: Option<&str>) -> io::Result<Option<Pick>> {
    let mut typed = String::new();
    // Starts on the agent picked now.
    let mut selected = current
        .and_then(|name| installed.iter().position(|a| a.name == name))
        .map_or(0, |at| at + 1);
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), crossterm::terminal::SetTitle("Agent"));
    let result = (|| loop {
        let (sections, picks) = listing(installed, &typed, current);
        selected = selected.min(picks.len().saturating_sub(1));
        terminal.draw(|f| {
            popover::draw(
                f,
                &popover::View {
                    chip: "Agent",
                    typed: &typed,
                    placeholder: "Search installed agents…",
                    sections: &sections,
                    selected,
                    empty: "No installed agent matches.",
                    footer: "enter send questions here · esc close",
                },
            )
        })?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match popover::edit(
            key.code,
            key.modifiers,
            &mut typed,
            &mut selected,
            picks.len(),
        ) {
            Edit::Quit => return Ok(None),
            Edit::Choose => {
                if let Some(pick) = picks.get(selected) {
                    return Ok(Some(pick.clone()));
                }
            }
            Edit::Complete | Edit::Typed | Edit::Moved | Edit::Nothing => {}
        }
    })();
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn installed() -> Vec<Installed> {
        ["claude", "codex", "gemini"]
            .into_iter()
            .map(|name| Installed {
                name,
                path: PathBuf::from(format!("/bin/{name}")),
            })
            .collect()
    }

    #[test]
    fn the_picked_agent_is_checked_and_search_narrows() {
        let (sections, picks) = listing(&installed(), "", Some("codex"));
        assert_eq!(picks[0], Pick::Any);
        assert_eq!(picks.len(), 4);
        let checked: Vec<_> = sections
            .iter()
            .flat_map(|s| &s.rows)
            .filter(|r| r.checked)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(checked, ["codex"]);
        let (_, picks) = listing(&installed(), "gem", None);
        assert_eq!(picks, [Pick::Agent("gemini")]);
    }

    #[test]
    fn without_a_default_any_agent_pane_is_checked() {
        let (sections, _) = listing(&installed(), "", None);
        assert!(sections[0].rows[0].checked);
    }
}
