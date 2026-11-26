//! What the reader does from a key, and the keys it does it from until
//! `[keys.reader]` in `config.toml` says otherwise.

use crate::keymap::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderAction {
    Help,
    Guide,
    GoToPage,
    Outline,
    OutlineWindow,
    Bookmark,
    Bookmarks,
    Find,
    NextMatch,
    PreviousMatch,
    Quit,
    Highlight,
    Ask,
    ScrollDown,
    ScrollUp,
    HalfScreenDown,
    HalfScreenUp,
    ScreenDown,
    ScreenUp,
    NextPage,
    PreviousPage,
    LinkBack,
    LinkForward,
    First,
    Last,
    Images,
    Look,
    Pen,
    Eraser,
    MoveInk,
    ReadAloud,
    StopReading,
    Voices,
    Slower,
    Faster,
    Narrower,
    Wider,
    FitWidth,
    PreviousSentence,
    NextSentence,
}

impl Action for ReaderAction {
    const VIEW: &'static str = "reader";
    /// `next_match` comes before `next_page`: `n` finds the next match while
    /// a search has one on the page, and turns the page otherwise.
    const TABLE: &'static [(&'static str, Self, &'static [&'static str])] = &[
        ("help", ReaderAction::Help, &["?", "/"]),
        ("guide", ReaderAction::Guide, &["!"]),
        ("go_to_page", ReaderAction::GoToPage, &[":"]),
        ("outline", ReaderAction::Outline, &["t"]),
        ("outline_window", ReaderAction::OutlineWindow, &["T"]),
        ("bookmark", ReaderAction::Bookmark, &["m"]),
        ("bookmarks", ReaderAction::Bookmarks, &["'"]),
        ("find", ReaderAction::Find, &["f", "F"]),
        ("next_match", ReaderAction::NextMatch, &["n"]),
        ("previous_match", ReaderAction::PreviousMatch, &["N"]),
        ("quit", ReaderAction::Quit, &["q"]),
        ("highlight", ReaderAction::Highlight, &["y"]),
        ("ask", ReaderAction::Ask, &["@", "ctrl+enter"]),
        ("scroll_down", ReaderAction::ScrollDown, &["j", "down"]),
        ("scroll_up", ReaderAction::ScrollUp, &["k", "up"]),
        (
            "half_screen_down",
            ReaderAction::HalfScreenDown,
            &["ctrl+d"],
        ),
        (
            "half_screen_up",
            ReaderAction::HalfScreenUp,
            &["u", "ctrl+u"],
        ),
        (
            "screen_down",
            ReaderAction::ScreenDown,
            &["space", "pagedown"],
        ),
        ("screen_up", ReaderAction::ScreenUp, &["b", "pageup"]),
        ("next_page", ReaderAction::NextPage, &["l", "n", "right"]),
        (
            "previous_page",
            ReaderAction::PreviousPage,
            &["h", "p", "left"],
        ),
        ("link_back", ReaderAction::LinkBack, &["H"]),
        ("link_forward", ReaderAction::LinkForward, &["L"]),
        ("first", ReaderAction::First, &["g", "home"]),
        ("last", ReaderAction::Last, &["G", "end"]),
        ("images", ReaderAction::Images, &["i"]),
        ("look", ReaderAction::Look, &["o"]),
        ("pen", ReaderAction::Pen, &["d", "D"]),
        ("eraser", ReaderAction::Eraser, &["e", "E"]),
        ("move_ink", ReaderAction::MoveInk, &["v", "V"]),
        ("read_aloud", ReaderAction::ReadAloud, &["a"]),
        ("stop_reading", ReaderAction::StopReading, &["A"]),
        ("voices", ReaderAction::Voices, &["s", "S"]),
        ("slower", ReaderAction::Slower, &["["]),
        ("faster", ReaderAction::Faster, &["]"]),
        ("narrower", ReaderAction::Narrower, &["<", "-"]),
        ("wider", ReaderAction::Wider, &[">", "+"]),
        ("fit_width", ReaderAction::FitWidth, &["="]),
        ("previous_sentence", ReaderAction::PreviousSentence, &[","]),
        ("next_sentence", ReaderAction::NextSentence, &["."]),
    ];
}

#[cfg(test)]
mod tests {
    use super::ReaderAction;
    use crate::keymap::{Action, Keymap};

    #[test]
    fn every_action_is_listed_once_and_its_defaults_parse() {
        let keys = Keymap::<ReaderAction>::default();
        for (i, (name, action, _)) in ReaderAction::TABLE.iter().enumerate() {
            assert!(
                ReaderAction::TABLE[i + 1..]
                    .iter()
                    .all(|(other, theirs, _)| other != name && theirs != action),
                "{name}"
            );
            assert!(!keys.label(*action).is_empty(), "{name}");
        }
    }
}
