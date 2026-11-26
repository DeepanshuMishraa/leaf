//! What the diff viewer does from a key, and the keys it does it from until
//! `[keys.diff]` in `config.toml` says otherwise.

use crate::keymap::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffAction {
    TabSummary,
    TabTimeline,
    TabComments,
    TabChecks,
    Quit,
    QuitNow,
    FileNote,
    GlobalNote,
    Viewed,
    SendReview,
    Overview,
    Down,
    Up,
    HalfPageDown,
    HalfPageUp,
    PageDown,
    PageUp,
    First,
    Last,
    NextFile,
    PreviousFile,
    Files,
    Outline,
    OutlineWindow,
    Select,
    Comment,
    Ask,
    Comments,
    Copy,
    CopyAll,
    OtherSide,
    Layout,
    Reload,
    Fold,
    Unfold,
    ScrollLeft,
    ScrollRight,
    ScrollReset,
    Help,
}

impl Action for DiffAction {
    const VIEW: &'static str = "diff";
    /// The `tab_*` actions come first: in a pull request's Code tab they take
    /// `tab`, `←` and `→`, which otherwise open the overview and scroll.
    const TABLE: &'static [(&'static str, Self, &'static [&'static str])] = &[
        ("tab_summary", DiffAction::TabSummary, &["1"]),
        ("tab_timeline", DiffAction::TabTimeline, &["2"]),
        (
            "tab_comments",
            DiffAction::TabComments,
            &["3", "left", "backtab"],
        ),
        ("tab_checks", DiffAction::TabChecks, &["5", "right", "tab"]),
        ("quit", DiffAction::Quit, &["q"]),
        ("quit_now", DiffAction::QuitNow, &["ctrl+c"]),
        ("file_note", DiffAction::FileNote, &["F"]),
        ("global_note", DiffAction::GlobalNote, &["M"]),
        ("viewed", DiffAction::Viewed, &["w"]),
        ("send_review", DiffAction::SendReview, &["S"]),
        ("overview", DiffAction::Overview, &["tab", "O"]),
        ("down", DiffAction::Down, &["j", "down"]),
        ("up", DiffAction::Up, &["k", "up"]),
        ("half_page_down", DiffAction::HalfPageDown, &["d", "ctrl+d"]),
        ("half_page_up", DiffAction::HalfPageUp, &["u", "ctrl+u"]),
        ("page_down", DiffAction::PageDown, &["space", "pagedown"]),
        ("page_up", DiffAction::PageUp, &["b", "pageup"]),
        ("first", DiffAction::First, &["g", "home"]),
        ("last", DiffAction::Last, &["G", "end"]),
        ("next_file", DiffAction::NextFile, &["]", "n"]),
        ("previous_file", DiffAction::PreviousFile, &["[", "N"]),
        ("files", DiffAction::Files, &["f"]),
        ("outline", DiffAction::Outline, &["t"]),
        ("outline_window", DiffAction::OutlineWindow, &["T"]),
        ("select", DiffAction::Select, &["V", "v"]),
        ("comment", DiffAction::Comment, &["c", "C"]),
        ("ask", DiffAction::Ask, &["@", "ctrl+enter"]),
        ("comments", DiffAction::Comments, &["m"]),
        ("copy", DiffAction::Copy, &["y"]),
        ("copy_all", DiffAction::CopyAll, &["Y"]),
        ("other_side", DiffAction::OtherSide, &["o"]),
        ("layout", DiffAction::Layout, &["i"]),
        ("reload", DiffAction::Reload, &["r"]),
        ("fold", DiffAction::Fold, &["h"]),
        ("unfold", DiffAction::Unfold, &["l"]),
        ("scroll_left", DiffAction::ScrollLeft, &["H", "left"]),
        ("scroll_right", DiffAction::ScrollRight, &["L", "right"]),
        ("scroll_reset", DiffAction::ScrollReset, &["0"]),
        ("help", DiffAction::Help, &["?"]),
    ];
}

#[cfg(test)]
mod tests {
    use super::DiffAction;
    use crate::keymap::{Action, Keymap};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn every_action_is_listed_once_and_its_defaults_parse() {
        let keys = Keymap::<DiffAction>::default();
        for (i, (name, action, _)) in DiffAction::TABLE.iter().enumerate() {
            assert!(
                DiffAction::TABLE[i + 1..]
                    .iter()
                    .all(|(other, theirs, _)| other != name && theirs != action),
                "{name}"
            );
            assert!(!keys.label(*action).is_empty(), "{name}");
        }
    }

    #[test]
    fn ctrl_c_quits_before_it_comments() {
        let keys = Keymap::<DiffAction>::default();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            keys.actions(&ctrl_c),
            vec![DiffAction::QuitNow, DiffAction::Comment]
        );
    }
}
