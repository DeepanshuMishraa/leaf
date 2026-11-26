//! What `leaf pr` does from a key, and the keys it does it from until
//! `[keys.pr_list]` and `[keys.pr]` in `config.toml` say otherwise. The list's
//! filter, the comment box and the merge, close and re-run menus keep their
//! own keys: letters type or pick there.

use crate::keymap::actions;

actions! {
    /// Letters filter the list, so its own keys are arrows, `tab`, `@` and `ctrl`.
    pub enum PrListAction in "pr_list", "The list of pull requests and issues (leaf pr)" {
        SwitchKind = "switch_kind" ["left", "right"] "pull requests or issues",
        SwitchStatus = "switch_status" ["tab", "backtab"] "open or done",
        Scope = "scope" ["@"] "everything, or only what's assigned to you",
        Reload = "reload" ["R", "ctrl+r"] "search again (R only while nothing's typed)",
        CopyNumber = "copy_number" ["ctrl+y"] "copy the picked number",
        PageDown = "page_down" ["pagedown"] "eight rows down",
        PageUp = "page_up" ["pageup"] "eight rows up",
    }
}

actions! {
    /// A key bound to several actions tries them in this order: on Checks,
    /// `enter` opens a job's log before it replies, `r` re-runs before it
    /// marks the pull request ready, and `o` opens the picked check.
    pub enum PrAction in "pr", "A pull request or issue (leaf pr)" {
        QuitNow = "quit_now" ["ctrl+c"] "quit at once",
        Escape = "escape" ["esc"] "drop the selection or close the log; then back to the list",
        Back = "back" ["backspace"] "Checks: close the log",
        Quit = "quit" ["q"] "close the log, or back to the list, or quit",
        Toggle = "toggle" ["enter", "space"] "Checks, in a log: open or close the group",
        OpenLog = "open_log" ["enter"] "Checks: open the job's log",
        Reply = "reply" ["enter"] "reply to the picked comment",
        Down = "down" ["j", "down"] "scroll down, or the next comment or job",
        Up = "up" ["k", "up"] "scroll up, or the previous comment or job",
        HalfScreenDown = "half_screen_down" ["ctrl+d"] "half a screen down",
        HalfScreenUp = "half_screen_up" ["ctrl+u"] "half a screen up",
        ScreenDown = "screen_down" ["space", "pagedown"] "a screen down",
        ScreenUp = "screen_up" ["b", "pageup"] "a screen up",
        Top = "top" ["g", "home"] "the top",
        Bottom = "bottom" ["G", "end"] "the bottom",
        NextComment = "next_comment" ["]"] "pick the next comment",
        PreviousComment = "previous_comment" ["["] "pick the previous comment",
        NextTab = "next_tab" ["tab"] "next tab, round to the first",
        PreviousTab = "previous_tab" ["backtab"] "previous tab, round to the last",
        RightTab = "right_tab" ["right", "l"] "next tab",
        LeftTab = "left_tab" ["left", "h"] "previous tab",
        Summary = "summary" ["1"] "Summary tab",
        Timeline = "timeline" ["2"] "Timeline tab",
        Comments = "comments" ["3"] "Comments tab",
        Code = "code" ["4"] "Code tab: review the diff",
        Checks = "checks" ["5"] "Checks tab",
        Expand = "expand" ["e"] "long comments and quotes, or a job's groups",
        Help = "help" ["?"] "list the keys",
        ReloadLog = "reload_log" ["R"] "Checks: fetch the log again",
        Reload = "reload" ["R"] "reload",
        OpenCheck = "open_check" ["o"] "Checks: open the picked check",
        Open = "open" ["o"] "open on GitHub",
        Copy = "copy" ["y"] "copy the checkout command, or an issue's link",
        CopyLink = "copy_link" ["Y"] "copy the link",
        CopyNumber = "copy_number" ["n"] "copy the number",
        Comment = "comment" ["c"] "comment",
        Merge = "merge" ["m"] "merge: squash, merge commit or rebase",
        Close = "close" ["x"] "close, or reopen a closed one",
        Rerun = "rerun" ["r"] "Checks: re-run jobs",
        Ready = "ready" ["r"] "ready for review, or back to draft",
        Approve = "approve" ["a"] "approve",
    }
}
