//! What `leaf board` does from a key, and the keys it does it from until
//! `[keys.board]` in `config.toml` says otherwise. Text being written and a
//! question being typed keep their own keys: letters type there.

use crate::keymap::actions;

actions! {
    /// A key with `ctrl` finds its own action before the plain key's, so
    /// `ctrl+c` quits while `c` asks, and `ctrl+v` pastes while `v` selects.
    pub enum BoardAction in "board", "The whiteboard (leaf board)" {
        QuitNow = "quit_now" ["ctrl+c"] "quit",
        Quit = "quit" ["q"] "quit",
        Escape = "escape" ["esc"] "let go of the selection and take the select tool",
        Copy = "copy" ["y"] "copy the selection",
        Cut = "cut" ["ctrl+x"] "cut the selection",
        Paste = "paste" ["ctrl+v"] "paste",
        Undo = "undo" ["u", "ctrl+z"] "undo",
        Redo = "redo" ["U", "ctrl+Z", "ctrl+r", "ctrl+y"] "redo",
        SelectAll = "select_all" ["ctrl+a"] "select everything",
        Duplicate = "duplicate" ["D", "ctrl+d"] "duplicate the selection",
        Export = "export" ["E", "ctrl+e"] "write the board as SVG and PNG",
        Group = "group" ["g", "ctrl+g"] "group the selection",
        Ungroup = "ungroup" ["G", "ctrl+G"] "ungroup the selection",
        Lock = "lock" ["L"] "lock the canvas against panning and zooming",
        Delete = "delete" ["x", "delete", "backspace"] "delete the selection",
        NoColor = "no_color" ["0"] "no background on the selection",
        Color1 = "color_1" ["1"] "first color",
        Color2 = "color_2" ["2"] "second color",
        Color3 = "color_3" ["3"] "third color",
        Color4 = "color_4" ["4"] "fourth color",
        Color5 = "color_5" ["5"] "fifth color",
        Color6 = "color_6" ["6"] "sixth color",
        Ask = "ask" ["@", "c"] "ask the agent to draw",
        Fit = "fit" ["f"] "fit the board on screen",
        ScrollMode = "scroll_mode" ["z"] "scrolling pans or zooms",
        ZoomIn = "zoom_in" ["+", "="] "zoom in",
        ZoomOut = "zoom_out" ["-"] "zoom out",
        ZoomInStep = "zoom_in_step" ["alt+up"] "zoom in a little",
        ZoomOutStep = "zoom_out_step" ["alt+down"] "zoom out a little",
        Fill = "fill" ["b"] "next fill",
        Background = "background" ["B"] "next background color",
        NextTool = "next_tool" ["tab"] "next tool",
        PreviousTool = "previous_tool" ["backtab"] "previous tool",
        Dash = "dash" ["s"] "solid, dashed or dotted line",
        StrokeWidth = "stroke_width" ["w"] "next line width",
        Roughness = "roughness" ["k"] "next roughness",
        Font = "font" ["n"] "next font",
        TextBigger = "text_bigger" [">"] "bigger text",
        TextSmaller = "text_smaller" ["<"] "smaller text",
        Arrowheads = "arrowheads" ["m"] "next arrowheads",
        Forward = "forward" ["]"] "bring forward",
        Backward = "backward" ["["] "send backward",
        Front = "front" ["}"] "bring to the front",
        Back = "back" ["{"] "send to the back",
        Dots = "dots" ["i"] "draw the board in pixels or dots",
        Help = "help" ["?"] "list the keys",
        Edit = "edit" ["enter"] "write in the selected text or shape",
        MoveLeft = "move_left" ["left"] "move the selection, or pan, left (shift: further)",
        MoveRight = "move_right" ["right"] "move right",
        MoveUp = "move_up" ["up"] "move up",
        MoveDown = "move_down" ["down"] "move down",
        ToolSelect = "tool_select" ["v"] "select tool",
        ToolHand = "tool_hand" ["h"] "hand tool",
        ToolBox = "tool_box" ["r"] "box tool",
        ToolDiamond = "tool_diamond" ["d"] "diamond tool",
        ToolEllipse = "tool_ellipse" ["o"] "ellipse tool",
        ToolArrow = "tool_arrow" ["a"] "arrow tool",
        ToolLine = "tool_line" ["l"] "line tool",
        ToolPen = "tool_pen" ["p"] "pen tool",
        ToolText = "tool_text" ["t"] "text tool",
        ToolFrame = "tool_frame" ["F"] "frame tool",
        ToolEraser = "tool_eraser" ["e"] "eraser tool",
    }
}

impl BoardAction {
    /// The color a `color_N` action gives the selection and what's drawn next.
    pub fn hue(self) -> Option<crate::hue::Hue> {
        let n = match self {
            BoardAction::Color1 => 0,
            BoardAction::Color2 => 1,
            BoardAction::Color3 => 2,
            BoardAction::Color4 => 3,
            BoardAction::Color5 => 4,
            BoardAction::Color6 => 5,
            _ => return None,
        };
        crate::hue::Hue::ALL.get(n).copied()
    }
}
