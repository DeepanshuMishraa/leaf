//! What the diff viewer does from a key, and the keys it does it from until
//! `[keys.diff]`, `[keys.diff_comments]`, `[keys.diff_overview]`,
//! `[keys.confirm_post]` and `[keys.verdict]` in `config.toml` say otherwise.

use crate::keymap::actions;

actions! {
    /// The `tab_*` actions come first: in a pull request's Code tab they take
    /// `tab`, `←` and `→`, which otherwise open the overview and scroll.
    pub enum DiffAction in "diff", "Reviewing a diff (leaf diff, leaf review, a pull request's Code tab)" {
        TabSummary = "tab_summary" ["1"] "pull request: Summary tab",
        TabTimeline = "tab_timeline" ["2"] "pull request: Timeline tab",
        TabComments = "tab_comments" ["3", "left", "backtab"] "pull request: Comments tab",
        TabChecks = "tab_checks" ["5", "right", "tab"] "pull request: Checks tab",
        Quit = "quit" ["q"] "quit; asks again when there are drafts",
        QuitNow = "quit_now" ["ctrl+c"] "quit at once",
        Escape = "escape" ["esc"] "drop the selection, then quit",
        FileNote = "file_note" ["F"] "note on this whole file",
        GlobalNote = "global_note" ["M"] "note on the whole change",
        Viewed = "viewed" ["w"] "mark the file viewed and fold it, or unmark it",
        SendReview = "send_review" ["S"] "send drafts and notes as one review",
        Overview = "overview" ["tab", "O"] "overview of every file",
        Down = "down" ["j", "down"] "next line",
        Up = "up" ["k", "up"] "previous line",
        HalfPageDown = "half_page_down" ["d", "ctrl+d"] "half a page down",
        HalfPageUp = "half_page_up" ["u", "ctrl+u"] "half a page up",
        PageDown = "page_down" ["space", "pagedown"] "a page down",
        PageUp = "page_up" ["b", "pageup"] "a page up",
        First = "first" ["g", "home"] "first line",
        Last = "last" ["G", "end"] "last line",
        NextFile = "next_file" ["]", "n"] "next file",
        PreviousFile = "previous_file" ["[", "N"] "previous file",
        Files = "files" ["f"] "file picker",
        Outline = "outline" ["t"] "outline of the files beside the diff",
        OutlineWindow = "outline_window" ["T"] "outline of the files over the diff",
        Select = "select" ["V", "v"] "visual mode: move to select lines",
        Comment = "comment" ["c", "C"] "comment on the line or selection",
        Ask = "ask" ["@", "ctrl+enter"] "ask the agent about the line or selection",
        Comments = "comments" ["m"] "list the comments",
        Copy = "copy" ["y"] "copy this range",
        CopyAll = "copy_all" ["Y"] "copy every comment",
        OtherSide = "other_side" ["o"] "other side of the diff",
        Layout = "layout" ["i"] "split or inline layout",
        Reload = "reload" ["r"] "reload the diff",
        Fold = "fold" ["h"] "fold the file to its heading",
        Unfold = "unfold" ["l"] "open the folded file",
        ScrollLeft = "scroll_left" ["H", "left"] "scroll left",
        ScrollRight = "scroll_right" ["L", "right"] "scroll right",
        ScrollReset = "scroll_reset" ["0"] "scroll back to the left edge",
        Help = "help" ["?"] "list the keys",
    }
}

actions! {
    pub enum CommentsAction in "diff_comments", "The diff's list of comments (m)" {
        Close = "close" ["esc", "q", "m"] "close the list",
        Down = "down" ["j", "down"] "next comment",
        Up = "up" ["k", "up"] "previous comment",
        Jump = "jump" ["enter"] "go to the comment",
        Delete = "delete" ["d"] "delete the comment",
        Github = "github" ["g"] "post it to GitHub",
        Agent = "agent" ["a"] "send it to the agent",
        Copy = "copy" ["y"] "copy it",
    }
}

actions! {
    /// `quit_now` comes before `file_note`: `ctrl+c` quits, `c` writes a note.
    pub enum OverviewAction in "diff_overview", "The diff's overview of every file (tab)" {
        QuitNow = "quit_now" ["ctrl+c"] "quit at once",
        Down = "down" ["j", "down"] "next file",
        Up = "up" ["k", "up"] "previous file",
        First = "first" ["g", "home"] "first row",
        Last = "last" ["G", "end"] "last row",
        Open = "open" ["enter", "l", "right"] "open the file, or write the global note",
        FileNote = "file_note" ["c"] "note on the file",
        GlobalNote = "global_note" ["C", "M"] "note on the whole change",
        Viewed = "viewed" ["w", "V"] "mark the file viewed, or unmark it",
        DeleteNote = "delete_note" ["x"] "delete the file's or the global note",
        SendReview = "send_review" ["S"] "send drafts and notes as one review",
        Comments = "comments" ["m"] "list the comments",
        Help = "help" ["?"] "list the keys",
        Close = "close" ["tab", "O", "esc"] "back to the diff",
        Quit = "quit" ["q"] "quit",
    }
}

actions! {
    pub enum ConfirmAction in "confirm_post", "Asked whether to post a comment to GitHub" {
        Yes = "yes" ["y", "Y", "enter"] "post it",
        No = "no" ["n", "N", "esc"] "back to the comment",
    }
}

actions! {
    pub enum VerdictAction in "verdict", "Picking a verdict, in the diff and in leaf plan (S)" {
        Approve = "approve" ["a"] "approve",
        ApproveWithNotes = "approve_with_notes" ["n"] "approve with notes",
        RequestChanges = "request_changes" ["c", "r"] "request changes",
        Cancel = "cancel" ["esc", "q"] "back without sending",
    }
}

#[cfg(test)]
mod tests {
    use super::DiffAction;
    use crate::keymap::Keymap;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
