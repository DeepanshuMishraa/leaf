//! What the reader does from a key, and the keys it does it from until
//! `[keys.reader]`, `[keys.ink]` and `[keys.bookmarks]` in `config.toml` say
//! otherwise.

use crate::hue::Hue;
use crate::keymap::actions;

actions! {
    /// `next_match` comes before `next_page`: `n` finds the next match while
    /// a search has one on the page, and turns the page otherwise.
    pub enum ReaderAction in "reader", "Reading a document" {
        Help = "help" ["?", "/"] "list the keys",
        Guide = "guide" ["!"] "how to highlight and draw",
        GoToPage = "go_to_page" [":"] "go to a page by number",
        Outline = "outline" ["t"] "outline beside the text",
        OutlineWindow = "outline_window" ["T"] "outline over the text",
        Bookmark = "bookmark" ["m"] "bookmark this page, or remove it",
        Bookmarks = "bookmarks" ["'"] "list the bookmarks",
        Find = "find" ["f", "F"] "find text",
        NextMatch = "next_match" ["n"] "next match, while a search has one on the page",
        PreviousMatch = "previous_match" ["N"] "previous match",
        Quit = "quit" ["q"] "quit",
        Highlight = "highlight" ["y"] "highlight the selection, or clear one",
        Ask = "ask" ["@", "ctrl+enter"] "ask an agent about the selection",
        ScrollDown = "scroll_down" ["j", "down"] "scroll down a line",
        ScrollUp = "scroll_up" ["k", "up"] "scroll up a line",
        HalfScreenDown = "half_screen_down" ["ctrl+d"] "half a screen down",
        HalfScreenUp = "half_screen_up" ["u", "ctrl+u"] "half a screen up",
        ScreenDown = "screen_down" ["space", "pagedown"] "a screen down",
        ScreenUp = "screen_up" ["b", "pageup"] "a screen up",
        NextPage = "next_page" ["l", "n", "right"] "next page, or picture in the folder",
        PreviousPage = "previous_page" ["h", "p", "left"] "previous page, or picture",
        LinkBack = "link_back" ["H"] "back through followed links",
        LinkForward = "link_forward" ["L"] "forward through followed links",
        First = "first" ["g", "home"] "first page, or the top",
        Last = "last" ["G", "end"] "last page, or the bottom",
        Images = "images" ["i"] "original or painted images",
        Look = "look" ["o"] "office documents: as designed or in the terminal's font",
        Pen = "pen" ["d", "D"] "take the pen",
        Eraser = "eraser" ["e", "E"] "take the eraser",
        MoveInk = "move_ink" ["v", "V"] "take the move tool, to pick and move ink",
        Escape = "escape" ["esc"] "drop the tool, selection or search; quit when there is none",
        Color1 = "color_1" ["1"] "yellow; paints the selection",
        Color2 = "color_2" ["2"] "green",
        Color3 = "color_3" ["3"] "blue",
        Color4 = "color_4" ["4"] "pink",
        Color5 = "color_5" ["5"] "purple",
        Color6 = "color_6" ["6"] "red",
        ReadAloud = "read_aloud" ["a"] "read aloud from here; pause and go on",
        StopReading = "stop_reading" ["A"] "stop reading aloud",
        Voices = "voices" ["s", "S"] "pick a voice",
        Slower = "slower" ["["] "read slower",
        Faster = "faster" ["]"] "read faster",
        Narrower = "narrower" ["<", "-"] "narrower column",
        Wider = "wider" [">", "+"] "wider column",
        FitWidth = "fit_width" ["="] "fit the column to the screen, or back",
        PreviousSentence = "previous_sentence" [","] "previous sentence aloud",
        NextSentence = "next_sentence" ["."] "next sentence aloud",
    }
}

impl ReaderAction {
    /// The color a `color_N` action picks.
    pub fn hue(self) -> Option<Hue> {
        let n = match self {
            ReaderAction::Color1 => 0,
            ReaderAction::Color2 => 1,
            ReaderAction::Color3 => 2,
            ReaderAction::Color4 => 3,
            ReaderAction::Color5 => 4,
            ReaderAction::Color6 => 5,
            _ => return None,
        };
        Hue::ALL.get(n).copied()
    }
}

actions! {
    /// With the move tool, while a stroke is picked.
    pub enum InkAction in "ink", "The move tool, with a stroke picked" {
        Left = "left" ["h", "left"] "move it left",
        Right = "right" ["l", "right"] "move it right",
        Up = "up" ["k", "up"] "move it up",
        Down = "down" ["j", "down"] "move it down",
        Grow = "grow" ["+", "="] "make it bigger",
        Shrink = "shrink" ["-"] "make it smaller",
        Delete = "delete" ["x", "delete", "backspace"] "delete it",
    }
}

actions! {
    pub enum BookmarksAction in "bookmarks", "The reader's list of bookmarks" {
        Close = "close" ["esc", "q", "'"] "close the list",
        Down = "down" ["j", "down"] "next bookmark",
        Up = "up" ["k", "up"] "previous bookmark",
        Open = "open" ["enter"] "go to the bookmark",
        Delete = "delete" ["x", "d", "delete", "backspace"] "remove the bookmark",
    }
}

#[cfg(test)]
mod tests {
    use super::ReaderAction;
    use crate::hue::Hue;
    use crate::keymap::Keymap;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn the_number_keys_pick_the_colors_they_always_did() {
        let keys = Keymap::<ReaderAction>::default();
        for c in '1'..='6' {
            let action = keys.actions(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            assert_eq!(action.first().and_then(|a| a.hue()), Hue::of_key(c), "{c}");
        }
    }
}
