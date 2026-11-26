//! What `leaf plan` does from a key, and the keys it does it from until
//! `[keys.plan]`, `[keys.plan_notes]` and `[keys.plan_quit]` in `config.toml`
//! say otherwise.

use crate::keymap::actions;

actions! {
    /// `quit_now` comes before `comment`: `ctrl+c` quits, `c` comments.
    pub enum PlanAction in "plan", "Reviewing a plan (leaf plan)" {
        QuitNow = "quit_now" ["ctrl+c"] "quit at once",
        Down = "down" ["j", "down"] "next line",
        Up = "up" ["k", "up"] "previous line",
        NextBlock = "next_block" ["]", "}"] "next block",
        PreviousBlock = "previous_block" ["[", "{"] "previous block",
        NextNoted = "next_noted" ["J"] "next block with notes",
        PreviousNoted = "previous_noted" ["K"] "previous block with notes",
        HalfScreenDown = "half_screen_down" ["ctrl+d"] "half a screen down",
        HalfScreenUp = "half_screen_up" ["ctrl+u"] "half a screen up",
        Select = "select" ["V", "v"] "select lines",
        BlockComment = "block_comment" ["B"] "comment on the whole block",
        ScreenDown = "screen_down" ["space", "pagedown"] "a screen down",
        ScreenUp = "screen_up" ["b", "pageup"] "a screen up",
        First = "first" ["g", "home"] "the global note",
        Last = "last" ["G", "end"] "the last block",
        Comment = "comment" ["c", "enter"] "comment on the line or the selection",
        GlobalNote = "global_note" ["C"] "global note on the whole plan",
        Remove = "remove" ["x"] "remove the selection, or mark the block (again: keep it)",
        Replace = "replace" ["r"] "replace the selection or the block with your text",
        Clear = "clear" ["X"] "clear the block's notes",
        NextChange = "next_change" ["D"] "next block that changed since the last round",
        Notes = "notes" ["L"] "list the notes on blocks",
        SendReview = "send_review" ["S"] "send the review with a verdict",
        Help = "help" ["?"] "list the keys",
        Escape = "escape" ["esc"] "drop the selection; quit when there is none",
        Quit = "quit" ["q"] "quit; asks first when there are notes",
    }
}

actions! {
    pub enum PlanNotesAction in "plan_notes", "leaf plan's list of notes (L)" {
        Close = "close" ["esc", "q", "L"] "close the list",
        Down = "down" ["j", "down"] "next note",
        Up = "up" ["k", "up"] "previous note",
        Open = "open" ["enter"] "go to the note",
        Edit = "edit" ["e"] "edit the note",
        Delete = "delete" ["x", "d"] "delete the note",
    }
}

actions! {
    /// Any other key keeps the plan open.
    pub enum PlanQuitAction in "plan_quit", "leaf plan asking whether to quit with notes unsent" {
        Quit = "quit" ["q", "y"] "quit without sending",
        SendReview = "send_review" ["S"] "send the review instead",
    }
}
