//! A review comment and where it sits in a diff.

/// The side of a diff a comment is anchored to. Removed lines only exist on
/// the old side; added and context lines exist on the new side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Old => "old",
            Side::New => "new",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "old" => Some(Side::Old),
            "new" => Some(Side::New),
            _ => None,
        }
    }
}

/// What happened to a comment after it was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Saved but not delivered anywhere yet.
    Draft,
    /// Pasted into an agent's prompt.
    Agent,
    /// Posted as a GitHub pull request review comment.
    Github,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Draft => "draft",
            State::Agent => "agent",
            State::Github => "github",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(State::Draft),
            "agent" => Some(State::Agent),
            "github" => Some(State::Github),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Draft => "draft",
            State::Agent => "sent to agent",
            State::Github => "on GitHub",
        }
    }
}

/// A range of lines that GitHub can attach a review comment to. GitHub needs
/// both ends on the same side and in the same hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub file: String,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
}

impl Anchor {
    pub fn range_label(&self) -> String {
        if self.start_line == self.end_line {
            format!("L{}", self.start_line)
        } else {
            format!("L{}-L{}", self.start_line, self.end_line)
        }
    }

    pub fn label(&self) -> String {
        format!("{} {}", self.file, self.range_label())
    }
}

/// A comment on a range of lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub anchor: Anchor,
    pub body: String,
    pub state: State,
}

impl Comment {
    pub fn label(&self) -> String {
        self.anchor.label()
    }
}
