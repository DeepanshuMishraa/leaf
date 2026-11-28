//! What the spreadsheet grid (`sheetview`) does from a key, and the keys it
//! does it from until `[keys.sheet]` in `config.toml` says otherwise.

use crate::keymap::actions;

actions! {
    pub enum SheetAction in "sheet", "A spreadsheet or CSV file (leaf open data.csv)" {
        Quit = "quit" ["q", "ctrl+c"] "quit",
        Escape = "escape" ["esc"] "drop the selection, then the filter",
        Left = "left" ["h", "left"] "the cell to the left",
        Down = "down" ["j", "down", "enter"] "the cell below",
        Up = "up" ["k", "up"] "the cell above",
        Right = "right" ["l", "right", "tab"] "the cell to the right",
        SelectLeft = "select_left" ["H", "shift+left"] "grow the selection left",
        SelectDown = "select_down" ["J", "shift+down"] "grow the selection down",
        SelectUp = "select_up" ["K", "shift+up"] "grow the selection up",
        SelectRight = "select_right" ["L", "shift+right"] "grow the selection right",
        RowStart = "row_start" ["0", "home"] "the first column",
        RowEnd = "row_end" ["$", "end"] "the last column",
        Top = "top" ["g"] "the first row",
        Bottom = "bottom" ["G"] "the last row",
        HalfScreenDown = "half_screen_down" ["ctrl+d"] "half a screen down",
        HalfScreenUp = "half_screen_up" ["ctrl+u"] "half a screen up",
        ScreenDown = "screen_down" ["space", "pagedown"] "a screen down",
        ScreenUp = "screen_up" ["b", "pageup"] "a screen up",
        NextSheet = "next_sheet" ["]"] "the next sheet",
        PreviousSheet = "previous_sheet" ["["] "the previous sheet",
        Find = "find" ["/", "f"] "find text in the cells",
        NextMatch = "next_match" ["n"] "the next match",
        PreviousMatch = "previous_match" ["N"] "the previous match",
        GoTo = "go_to" [":"] "go to a cell, such as B12, or a row number",
        Sort = "sort" ["s"] "sort by the column: up, down, then as it was",
        Filter = "filter" ["F", "|"] "show only rows whose cell in the column holds some text",
        Wider = "wider" [">"] "widen the column",
        Narrower = "narrower" ["<"] "narrow the column",
        Copy = "copy" ["y"] "copy the selection as tab-separated text",
        Help = "help" ["?"] "list the keys",
    }
}
