//! An outline: a tree of the places a document can be gone to, the panel over it, and
//! the way between its entries.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render;

/// Where an entry goes, as an index only the view can resolve into a place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locator {
    /// The heading the renderer numbered `n`, on the page being read.
    Text(usize),
    /// A file's index in a diff's files.
    Diff(usize),
    /// The page a document's own bookmark goes to, 0-based.
    Page(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineNode {
    pub label: String,
    pub depth: usize,
    /// `None` for a group, which holds no place of its own.
    pub locator: Option<Locator>,
    /// The words a filter finds the entry by: a diff file's path, any other entry's label.
    pub found_by: String,
}

/// Which entry the text is under is not kept here: it comes from the text.
#[derive(Debug, Clone, Default)]
pub struct Outline {
    pub nodes: Vec<OutlineNode>,
    folded: Vec<bool>,
    /// Typed only while the panel is typing.
    pub filter: String,
}

impl Outline {
    pub fn new(nodes: Vec<OutlineNode>) -> Self {
        Outline {
            folded: vec![false; nodes.len()],
            nodes,
            filter: String::new(),
        }
    }

    /// Brings in `nodes` as the tree, keeping what's typed. `renewed` says the text
    /// moved to another page even though the entries read the same.
    fn retarget(&mut self, nodes: Vec<OutlineNode>, renewed: bool) -> bool {
        if !renewed && nodes == self.nodes {
            return false;
        }
        self.folded = vec![false; nodes.len()];
        self.nodes = nodes;
        true
    }

    pub fn at(&self, locator: Locator) -> Option<usize> {
        self.nodes
            .iter()
            .position(|node| node.locator == Some(locator))
    }

    pub fn has_children(&self, at: usize) -> bool {
        self.nodes
            .get(at + 1)
            .is_some_and(|below| below.depth > self.nodes[at].depth)
    }

    /// A fold holds nothing back while a filter is typed, and takes hold again after it.
    pub fn folded(&self, at: usize) -> bool {
        self.filter.is_empty() && self.folded.get(at).copied().unwrap_or(false)
    }

    /// An entry with nothing under it stays as it is.
    pub fn fold(&mut self, at: usize, folded: bool) {
        let open = folded && self.has_children(at);
        if let Some(slot) = self.folded.get_mut(at) {
            *slot = open;
        }
    }

    /// Folds or unfolds the entry the panel marks for `at`, so one hidden by a group opens
    /// that group first; on a leaf it is the group the leaf hangs under, and a second press
    /// climbs to the next ancestor.
    pub fn fold_from(&mut self, at: usize, folded: bool) {
        let mut at = self.shown(Some(at));
        while let Some(node) = at {
            if self.has_children(node) && self.folded(node) != folded {
                self.fold(node, folded);
                return;
            }
            at = self.parent(node);
        }
    }

    /// The entries a panel shows, top to bottom: what the filter matches and the
    /// entries above them, less what a fold hides.
    pub fn visible(&self) -> Vec<usize> {
        self.listed().0
    }

    /// The entries a panel shows, with how many of them the filter matches.
    fn listed(&self) -> (Vec<usize>, usize) {
        let (mut rows, mut matched) = (Vec::new(), 0);
        self.walk(|at, is| {
            rows.push(at);
            matched += usize::from(is);
        });
        (rows, matched)
    }

    /// Visits the entries a panel shows in order, saying of each whether the filter
    /// matches it.
    fn walk(&self, mut visit: impl FnMut(usize, bool)) {
        let typed = self.typed();
        // Per entry: whether the filter matches it, and whether a panel lists it.
        let mut state: Vec<(bool, bool)> = self
            .nodes
            .iter()
            .map(|node| {
                (
                    node.locator.is_some()
                        && (typed.is_empty() || rank(&node.found_by, &typed).is_some()),
                    false,
                )
            })
            .collect();
        // The entries above the one at hand, outermost first; a match shows them.
        let mut above: Vec<(usize, usize)> = Vec::new();
        for (at, node) in self.nodes.iter().enumerate() {
            while above.last().is_some_and(|&(depth, _)| depth >= node.depth) {
                above.pop();
            }
            for &(_, up) in &above {
                state[up].1 |= state[at].0;
            }
            state[at].1 |= state[at].0;
            above.push((node.depth, at));
        }
        // The depth of a folded entry, while what is under it is passed over.
        let mut folded: Option<usize> = None;
        for (at, node) in self.nodes.iter().enumerate() {
            if folded.is_some_and(|depth| node.depth > depth) {
                continue;
            }
            folded = None;
            let (matched, listed) = state[at];
            if listed {
                visit(at, matched);
                if self.folded(at) {
                    folded = Some(node.depth);
                }
            }
        }
    }

    /// The filter as it is matched, lowercased.
    fn typed(&self) -> Vec<char> {
        self.filter.chars().flat_map(char::to_lowercase).collect()
    }

    /// The matches, top to bottom.
    fn stops(&self) -> Vec<usize> {
        let mut stops = Vec::new();
        self.walk(|at, matched| {
            if matched {
                stops.push(at);
            }
        });
        stops
    }

    /// The match to pick first: the one the typed characters lie closest together in,
    /// then the one where they start a word rather than the middle of one, then the earliest.
    pub fn best(&self) -> Option<usize> {
        let typed = self.typed();
        let mut best: Option<((usize, bool, usize), usize)> = None;
        self.walk(|at, matched| {
            if !matched {
                return;
            }
            let Some(rank) = rank(&self.nodes[at].found_by, &typed) else {
                return;
            };
            if best.is_none_or(|(been, _)| rank < been) {
                best = Some((rank, at));
            }
        });
        best.map(|(_, at)| at)
    }

    /// The entry `delta` places along the stops from `from`, which need not be one
    /// of them, and none when no stop lies that way. From none, the first, or the
    /// last when stepping up.
    pub fn step(&self, from: Option<usize>, delta: isize) -> Option<usize> {
        let stops = self.stops();
        if stops.is_empty() {
            return None;
        }
        let Some(from) = from else {
            return stops
                .first()
                .copied()
                .filter(|_| delta > 0)
                .or_else(|| stops.last().copied());
        };
        let on = usize::from(stops.contains(&from));
        let below = stops.partition_point(|&stop| stop < from);
        if delta > 0 {
            stops.get(below + on + delta as usize - 1).copied()
        } else {
            below
                .checked_sub(-delta as usize)
                .and_then(|index| stops.get(index).copied())
        }
    }

    /// The entry a panel marks for the one the text is under: itself while it is shown,
    /// else the nearest shown one above it.
    pub fn shown(&self, at: Option<usize>) -> Option<usize> {
        self.shown_in(&self.visible(), at)
    }

    fn shown_in(&self, visible: &[usize], at: Option<usize>) -> Option<usize> {
        let mut at = at?;
        while visible.binary_search(&at).is_err() {
            at = self.parent(at)?;
        }
        Some(at)
    }

    fn mark(&self, at: usize) -> &'static str {
        match (self.has_children(at), self.folded(at)) {
            (false, _) => "",
            (true, false) => "▾ ",
            (true, true) => "▸ ",
        }
    }

    fn first(&self, here: Option<usize>, rows: usize, visible: &[usize]) -> usize {
        here.and_then(|here| visible.iter().position(|&at| at == here))
            .unwrap_or(0)
            .saturating_sub(rows.saturating_sub(1))
            .min(visible.len().saturating_sub(rows))
    }

    /// Shows `at`: unfolds what hides it.
    pub fn reveal(&mut self, at: usize) {
        let mut above = self.parent(at);
        while let Some(up) = above {
            self.folded[up] = false;
            above = self.parent(up);
        }
    }

    pub fn parent(&self, at: usize) -> Option<usize> {
        (0..at)
            .rev()
            .find(|&above| self.nodes[above].depth < self.nodes[at].depth)
    }
}

/// How well `typed`, lowercased, names a label by its characters in order, whatever lies
/// between them: the spread of the match, whether it starts a word, and where it starts.
/// The less of each the better. None when the characters don't all come.
fn rank(label: &str, typed: &[char]) -> Option<(usize, bool, usize)> {
    if typed.is_empty() {
        return Some((0, false, 0));
    }
    let lower = || label.chars().flat_map(char::to_lowercase);
    let mut best: Option<(usize, bool, usize)> = None;
    for (start, lead) in lower().enumerate() {
        if lead != typed[0] {
            continue;
        }
        let mut after = lower().skip(start + 1);
        let (mut end, mut rest) = (start, start + 1);
        let mut whole = true;
        for &want in &typed[1..] {
            match after.position(|have| have == want) {
                Some(found) => {
                    end = rest + found;
                    rest = end + 1;
                }
                None => {
                    whole = false;
                    break;
                }
            }
        }
        let inside_a_word = start > 0 && lower().nth(start - 1).is_some_and(char::is_alphanumeric);
        if whole && best.is_none_or(|been| (end - start, inside_a_word, start) < been) {
            best = Some((end - start, inside_a_word, start));
        }
    }
    best
}
fn dim() -> Style {
    Style::default().fg(render::DIM)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineForm {
    Sidebar,
    Window,
}

pub struct OpenOutline {
    pub tree: Outline,
    /// The entry the text was last under; typing a filter isn't a move.
    pub followed: Option<usize>,
    /// Whether keys go to the filter; while it is on, every letter is typed.
    pub typing: bool,
    /// While a filter is typed, the match `Enter` goes to.
    pub picked: Option<usize>,
    /// The reader's alone: the entry the panel last jumped to, and where the text landed.
    /// The end of the document can pull a jump back mid-screen, so the highlight follows
    /// this until the text moves.
    pub pinned: Option<Pinned>,
    /// The reader's alone: the page whose headings the tree holds, `None` for the document's
    /// own bookmarks.
    pub page: Option<usize>,
}

#[derive(Clone, Copy)]
pub struct Pinned {
    pub node: usize,
    pub landed: (usize, usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Ask {
    Close,
    /// Show the outline in this form, switching to it if it shows in the other.
    Show(OutlineForm),
    /// The entry to go to, if the tree has one to move to.
    Go(Option<usize>),
}

impl OpenOutline {
    pub fn new(tree: Outline) -> Self {
        OpenOutline {
            tree,
            followed: None,
            typing: false,
            picked: None,
            pinned: None,
            page: None,
        }
    }

    /// Brings `nodes` as the tree, keeping what's typed. `renewed` says the text moved to
    /// another page even though the entries read the same. What was followed, picked or
    /// pinned belongs to the tree before.
    pub fn retarget(&mut self, nodes: Vec<OutlineNode>, renewed: bool) -> bool {
        if !self.tree.retarget(nodes, renewed) {
            return false;
        }
        self.followed = None;
        self.pinned = None;
        if self.typing {
            self.picked = self.tree.best();
        }
        true
    }

    /// `window`: whether the outline's own dialog is open.
    pub fn form(&self, window: bool) -> OutlineForm {
        if window {
            OutlineForm::Window
        } else {
            OutlineForm::Sidebar
        }
    }

    /// When the text moves under another entry, what hides it unfolds.
    pub fn follows(&mut self, here: Option<usize>) {
        if self.followed == here {
            return;
        }
        self.followed = here;
        if let Some(here) = here {
            self.tree.reveal(here);
        }
    }

    /// A key over the open outline: whether it is taken, and what it asks of the view.
    pub fn key(&mut self, key: &KeyEvent) -> (bool, Option<Ask>) {
        if self.typing {
            return self.type_key(key.code, key.modifiers.contains(KeyModifiers::CONTROL));
        }
        match key.code {
            KeyCode::Char('/') => {
                self.tree.filter.clear();
                self.picked = None;
                self.typing = true;
                (true, None)
            }
            KeyCode::Esc | KeyCode::Char('q') => (true, Some(Ask::Close)),
            KeyCode::Char('t') => (true, Some(Ask::Show(OutlineForm::Sidebar))),
            KeyCode::Char('T') => (true, Some(Ask::Show(OutlineForm::Window))),
            KeyCode::Down | KeyCode::Char('j') => {
                (true, Some(Ask::Go(self.tree.step(self.followed, 1))))
            }
            KeyCode::Up | KeyCode::Char('k') => {
                (true, Some(Ask::Go(self.tree.step(self.followed, -1))))
            }
            KeyCode::Home | KeyCode::Char('g') => (true, Some(Ask::Go(self.tree.step(None, 1)))),
            KeyCode::End | KeyCode::Char('G') => (true, Some(Ask::Go(self.tree.step(None, -1)))),
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(here) = self.followed {
                    self.tree.fold_from(here, false);
                }
                (true, None)
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(here) = self.followed {
                    self.tree.fold_from(here, true);
                }
                (true, None)
            }
            _ => (false, None),
        }
    }

    /// The keys of a filter being typed; every key is the outline's, letters among them.
    fn type_key(&mut self, code: KeyCode, ctrl: bool) -> (bool, Option<Ask>) {
        match code {
            KeyCode::Enter => {
                let picked = self.picked;
                self.stop_typing();
                return (true, Some(Ask::Go(picked)));
            }
            KeyCode::Esc => self.stop_typing(),
            KeyCode::Down | KeyCode::Tab => self.pick(1),
            KeyCode::Up | KeyCode::BackTab => self.pick(-1),
            KeyCode::Backspace => {
                self.tree.filter.pop();
                self.picked = self.tree.best();
            }
            KeyCode::Char('u') if ctrl => {
                self.tree.filter.clear();
                self.picked = self.tree.best();
            }
            KeyCode::Char(c) if !ctrl => {
                self.tree.filter.push(c);
                self.picked = self.tree.best();
            }
            _ => {}
        }
        (true, None)
    }

    fn pick(&mut self, delta: isize) {
        // Past the last match, or before the first, the pick stays.
        if let Some(next) = self.tree.step(self.picked, delta) {
            self.picked = Some(next);
        }
    }

    fn stop_typing(&mut self) {
        self.typing = false;
        self.tree.filter.clear();
        self.picked = None;
    }

    /// The panel over the outline: its head, the entries `rows` hold, the keys. `row`
    /// writes what follows an entry's fold; `empty` stands in when there are none.
    pub fn lines(
        &self,
        here: Option<usize>,
        rows: usize,
        empty: &'static str,
        row: impl Fn(&OutlineNode, Style, bool) -> Vec<Span<'static>>,
    ) -> Vec<Line<'static>> {
        let rows = rows.max(1);
        // While a filter is typed, the match picked is marked; with nothing typed yet, the
        // entry under the text still is.
        let (visible, matched) = self.tree.listed();
        let here = match self.picked {
            Some(picked) if self.typing => Some(picked),
            _ => self.tree.shown_in(&visible, here),
        };
        let mut lines = vec![self.head(visible.len(), matched)];
        if visible.is_empty() {
            let said = if self.tree.nodes.is_empty() {
                empty
            } else {
                "Nothing matches the filter."
            };
            lines.push(Line::from(Span::styled(said, dim())));
        }
        let first = self.tree.first(here, rows, &visible);
        for &at in visible.iter().skip(first).take(rows) {
            let entry = &self.tree.nodes[at];
            let picked = Some(at) == here;
            let style = if picked {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                dim()
            };
            let lead = format!("{}{}", "  ".repeat(entry.depth), self.tree.mark(at));
            let mut spans = vec![
                Span::styled(if picked { "› " } else { "  " }, style),
                Span::styled(lead, style),
            ];
            spans.extend(row(entry, style, picked));
            lines.push(Line::from(spans));
        }
        let mut keys = vec![Span::styled("j/k · h/l fold · / ", dim())];
        keys.push(Span::styled(
            "filter",
            if self.typing { Style::default() } else { dim() },
        ));
        lines.push(Line::from(keys));
        lines
    }

    /// The query and how many entries it matches, of those listed; the count of entries
    /// when nothing is typed.
    fn head(&self, shown: usize, matched: usize) -> Line<'static> {
        let typed = &self.tree.filter;
        if typed.is_empty() {
            return Line::from(Span::styled(format!("{shown} entries"), dim()));
        }
        let short: String = typed
            .chars()
            .rev()
            .take(16)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let quoted = if typed.chars().count() > 16 {
            format!("…{short}")
        } else {
            format!("“{typed}”")
        };
        let said = format!("{quoted} · {matched} of {}", self.tree.nodes.len());
        Line::from(Span::styled(said, dim()))
    }
}

/// Whether a heading has an entry: one with no words has none.
fn titled(heading: &render::Heading) -> bool {
    !heading.title.is_empty()
}

/// A heading that skips a level hangs under the nearest shallower one.
pub fn from_headings(headings: &[render::Heading]) -> Vec<OutlineNode> {
    let mut nodes = Vec::with_capacity(headings.len());
    // The levels of the entries the next heading hangs under.
    let mut open: Vec<u8> = Vec::new();
    for heading in headings.iter().filter(|heading| titled(heading)) {
        while open.last().is_some_and(|&level| level >= heading.level) {
            open.pop();
        }
        nodes.push(OutlineNode {
            label: heading.title.clone(),
            depth: open.len(),
            locator: Some(Locator::Text(heading.index)),
            found_by: heading.title.clone(),
        });
        open.push(heading.level);
    }
    nodes
}

/// One entry per bookmark, in order: the index `page_at` answers with.
pub fn from_bookmarks(bookmarks: &[crate::doc::Bookmark]) -> Vec<OutlineNode> {
    bookmarks
        .iter()
        .map(|bookmark| OutlineNode {
            label: bookmark.title.clone(),
            depth: bookmark.depth,
            locator: Some(Locator::Page(bookmark.page)),
            found_by: bookmark.title.clone(),
        })
        .collect()
}

/// The diff's files as a tree of directories. A directory is told by where it
/// hangs and its name; an absolute path hangs from a root with no name.
pub fn from_diff_files(files: &[crate::diff::File]) -> Vec<OutlineNode> {
    struct Branch {
        label: String,
        path: String,
        locator: Option<Locator>,
        children: Vec<usize>,
    }
    let mut branches: Vec<Branch> = Vec::new();
    let mut roots: Vec<usize> = Vec::new();
    // Where each directory met so far is, by the entry it hangs under.
    let mut dirs: HashMap<(Option<usize>, &str), usize> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        let path = file.path();
        let mut folders: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let name = folders.pop().unwrap_or(path);
        if path.starts_with('/') {
            folders.insert(0, "");
        }
        let mut above: Option<usize> = None;
        for part in folders {
            let at = match dirs.get(&(above, part)) {
                Some(&at) => at,
                None => {
                    let at = branches.len();
                    branches.push(Branch {
                        label: part.to_owned(),
                        path: part.to_owned(),
                        locator: None,
                        children: Vec::new(),
                    });
                    match above {
                        Some(above) => branches[above].children.push(at),
                        None => roots.push(at),
                    }
                    dirs.insert((above, part), at);
                    at
                }
            };
            above = Some(at);
        }
        let at = branches.len();
        branches.push(Branch {
            label: name.to_owned(),
            path: path.to_owned(),
            locator: Some(Locator::Diff(index)),
            children: Vec::new(),
        });
        match above {
            Some(above) => branches[above].children.push(at),
            None => roots.push(at),
        }
    }
    fn flatten(branches: &[Branch], at: usize, depth: usize, nodes: &mut Vec<OutlineNode>) {
        let branch = &branches[at];
        nodes.push(OutlineNode {
            label: branch.label.clone(),
            depth,
            locator: branch.locator,
            found_by: branch.path.clone(),
        });
        for &child in &branch.children {
            flatten(branches, child, depth + 1, nodes);
        }
    }
    let mut nodes = Vec::with_capacity(branches.len());
    for root in roots {
        flatten(&branches, root, 0, &mut nodes);
    }
    nodes
}

/// The last heading starting at or above `row`, or the first when the row is above
/// them all.
pub fn heading_at(headings: &[render::Heading], row: usize) -> Option<usize> {
    let reached = |heading: &render::Heading| titled(heading) && heading.row <= row;
    headings
        .iter()
        .rposition(reached)
        .or_else(|| headings.iter().position(titled))
        .map(|at| headings[at].index)
}

/// The bookmark at or before `page` that goes furthest; a page two share belongs to the
/// earlier one.
pub fn page_at(bookmarks: &[crate::doc::Bookmark], page: usize) -> Option<usize> {
    bookmarks
        .iter()
        .enumerate()
        .filter(|(_, bookmark)| bookmark.page <= page)
        .max_by_key(|&(at, bookmark)| (bookmark.page, std::cmp::Reverse(at)))
        .map(|(at, _)| at)
        .or_else(|| (!bookmarks.is_empty()).then_some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;

    /// Entries from (depth, label) pairs, numbered in order.
    fn entries(pairs: &[(usize, &str)]) -> Vec<OutlineNode> {
        pairs
            .iter()
            .enumerate()
            .map(|(at, (depth, label))| OutlineNode {
                label: (*label).to_owned(),
                depth: *depth,
                locator: Some(Locator::Text(at)),
                found_by: (*label).to_owned(),
            })
            .collect()
    }

    fn tree(pairs: &[(usize, &str)]) -> Outline {
        Outline::new(entries(pairs))
    }

    fn labels(outline: &Outline, visible: &[usize]) -> Vec<String> {
        visible
            .iter()
            .map(|&at| outline.nodes[at].label.clone())
            .collect()
    }

    fn heading(index: usize, level: u8, title: &str, row: usize) -> render::Heading {
        render::Heading {
            index,
            level,
            title: title.to_owned(),
            row,
        }
    }

    #[test]
    fn a_fold_hides_what_is_under_an_entry_and_leaves_the_rest() {
        let mut outline = tree(&[(0, "A"), (1, "B"), (2, "C"), (0, "D")]);
        assert_eq!(outline.visible(), vec![0, 1, 2, 3]);
        outline.fold(0, true);
        assert_eq!(labels(&outline, &outline.visible()), ["A", "D"]);
        // An entry with nothing under it has nothing to fold.
        outline.fold(3, true);
        assert!(!outline.folded(3));
        outline.fold(0, false);
        assert_eq!(outline.visible(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_filter_keeps_its_matches_and_the_entries_above_them() {
        let mut outline = tree(&[(0, "Intro"), (1, "Setup"), (2, "Notes"), (0, "End")]);
        outline.filter.push('n');
        assert_eq!(
            labels(&outline, &outline.visible()),
            ["Intro", "Setup", "Notes", "End"]
        );
        outline.filter.push('o');
        // What hangs under a match, or lies beside one, is no match itself.
        assert_eq!(
            labels(&outline, &outline.visible()),
            ["Intro", "Setup", "Notes"]
        );
        assert_eq!(outline.best(), Some(2));
        outline.filter.clear();
        outline.filter.push('z');
        assert!(outline.visible().is_empty());
        assert_eq!(outline.best(), None);
        outline.filter.pop();
        assert_eq!(outline.visible().len(), 4);
    }

    #[test]
    fn a_fold_and_a_filter_narrow_what_steps_go_through() {
        let mut outline = tree(&[(0, "A"), (1, "B"), (1, "C"), (0, "D")]);
        outline.fold(0, true);
        // Stepping down from the fold goes on past what it hides.
        assert_eq!(outline.step(Some(0), 1), Some(3));
        // Past the last stop, or before the first, a step goes nowhere.
        assert_eq!(outline.step(Some(3), 1), None);
        assert_eq!(outline.step(Some(0), -1), None);
        // From nowhere in what's shown, a step down starts at the top.
        assert_eq!(outline.step(None, 1), Some(0));
        assert_eq!(outline.step(None, -1), Some(3));
        outline.fold(0, false);
        // "C" keeps the entry above it, which a move passes over.
        outline.filter.push('c');
        assert_eq!(outline.step(None, 1), Some(2));
        assert_eq!(outline.step(Some(0), 1), Some(2));
        assert_eq!(outline.step(None, -1), Some(2));
    }

    #[test]
    fn revealing_an_entry_unfolds_it() {
        let mut outline = tree(&[(0, "A"), (1, "B"), (0, "C")]);
        outline.fold(0, true);
        assert_eq!(outline.visible(), vec![0, 2]);
        outline.reveal(1);
        assert_eq!(outline.visible(), vec![0, 1, 2]);
    }
    #[test]
    fn a_filter_matches_characters_in_order() {
        let mut outline = tree(&[
            (0, "Pull requests and issues"),
            (1, "Margin: review your agen"),
            (2, "Plans"),
            (2, "Changes"),
            (0, "Configuration"),
        ]);
        let find = |outline: &mut Outline, typed: &str| {
            outline.filter.clear();
            for c in typed.chars() {
                outline.filter.push(c);
            }
            (labels(outline, &outline.visible()), outline.best())
        };
        // A close match is picked over an earlier one whose characters lie scattered.
        let both: Vec<String> = [
            "Pull requests and issues",
            "Margin: review your agen",
            "Plans",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(find(&mut outline, "pla"), (both.clone(), Some(2)));
        assert_eq!(find(&mut outline, "pln"), (both, Some(2)));
        assert_eq!(
            find(&mut outline, "CFg"),
            (vec!["Configuration".to_owned()], Some(4))
        );
    }

    #[test]
    fn a_match_at_a_word_s_start_outranks_one_inside_a_word() {
        let mut outline = tree(&[(0, "Transport layer"), (0, "Preview ports")]);
        for c in "port".chars() {
            outline.filter.push(c);
        }
        assert_eq!(outline.best(), Some(1));
    }

    #[test]
    fn a_file_is_found_by_its_path_and_a_directory_only_places_it() {
        let dir = |label: &str, depth: usize| OutlineNode {
            label: label.to_owned(),
            depth,
            locator: None,
            found_by: label.to_owned(),
        };
        let file = |label: &str, depth: usize, index: usize, path: &str| OutlineNode {
            label: label.to_owned(),
            depth,
            locator: Some(Locator::Diff(index)),
            found_by: path.to_owned(),
        };
        let mut outline = Outline::new(vec![
            dir("src", 0),
            dir("core", 1),
            file("a.rs", 2, 0, "src/core/a.rs"),
            file("main.rs", 1, 1, "src/main.rs"),
            dir("docs", 0),
            file("x.md", 1, 2, "docs/x.md"),
        ]);
        let find = |outline: &mut Outline, typed: &str| {
            outline.filter.clear();
            for c in typed.chars() {
                outline.filter.push(c);
            }
            labels(outline, &outline.visible())
        };
        // The files under a directory are its matches; the directory only places them.
        assert_eq!(
            find(&mut outline, "src"),
            ["src", "core", "a.rs", "main.rs"]
        );
        assert_eq!(outline.step(None, 1), Some(2));
        assert_eq!(outline.step(Some(2), 1), Some(3));
        assert_eq!(find(&mut outline, "docs/x"), ["docs", "x.md"]);
        assert_eq!(outline.best(), Some(5));
    }
    #[test]
    fn a_step_from_an_entry_that_is_no_stop_goes_to_the_nearest_one() {
        let mut outline = tree(&[(0, "Start"), (1, "Zeta"), (1, "Middle"), (0, "Last")]);
        outline.filter.push('z');
        assert_eq!(outline.step(Some(3), -1), Some(1));
        assert_eq!(outline.step(Some(3), 1), None);
        assert_eq!(outline.step(Some(0), 1), Some(1));
    }

    #[test]
    fn the_filter_word_is_bright_while_a_filter_is_typed() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let mut open = OpenOutline::new(tree(&[(0, "Plans"), (0, "Rex")]));
        let keys = |open: &OpenOutline| {
            let line = open.lines(None, 6, "None", |_, _, _| vec![]).pop().unwrap();
            (line.to_string(), line.spans[1].style.fg)
        };
        let head =
            |open: &OpenOutline| open.lines(None, 6, "None", |_, _, _| vec![])[0].to_string();
        let quiet = ("j/k · h/l fold · / filter".to_owned(), Some(render::DIM));
        assert_eq!(keys(&open), quiet);
        assert_eq!(head(&open), "2 entries");
        open.key(&key(KeyCode::Char('/')));
        // Being typed: the word says so, and so does the query.
        assert_eq!(keys(&open), ("j/k · h/l fold · / filter".to_owned(), None));
        open.key(&key(KeyCode::Char('p')));
        assert_eq!(head(&open), "“p” · 1 of 2");
        open.key(&key(KeyCode::Enter));
        assert_eq!(keys(&open), quiet);
        assert_eq!(head(&open), "2 entries");
        // A slash and a q are letters of the query here, not keys.
        open.key(&key(KeyCode::Char('/')));
        open.key(&key(KeyCode::Char('r')));
        open.key(&key(KeyCode::Char('/')));
        open.key(&key(KeyCode::Char('q')));
        assert_eq!(open.tree.filter, "r/q");
    }

    #[test]
    fn the_panel_scrolls_to_keep_the_marked_entry() {
        let open = OpenOutline::new(tree(&[
            (0, "Zero"),
            (0, "One"),
            (0, "Two"),
            (0, "Three"),
            (0, "Four"),
            (0, "Five"),
            (0, "Six"),
            (0, "Seven"),
            (0, "Eight"),
            (0, "Nine"),
        ]));
        let shown = |here: Option<usize>| {
            open.lines(here, 3, "None", |entry, _, _| {
                vec![Span::raw(entry.label.clone())]
            })
            .iter()
            .skip(1)
            .take(3)
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
        };
        assert_eq!(shown(Some(1)), ["  Zero", "› One", "  Two"]);
        assert_eq!(shown(Some(8)), ["  Six", "  Seven", "› Eight"]);
    }

    #[test]
    fn a_path_is_typed_with_its_slashes_and_finds_its_file() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let files = from_diff_files(&changed(&["a/main.rs", "b/main.rs"]));
        let mut open = OpenOutline::new(Outline::new(files));
        open.key(&key(KeyCode::Char('/')));
        for c in "b/main".chars() {
            open.key(&key(KeyCode::Char(c)));
        }
        assert_eq!(open.tree.filter, "b/main");
        assert_eq!(open.picked, Some(3));
        assert_eq!(
            open.key(&key(KeyCode::Enter)),
            (true, Some(Ask::Go(Some(3))))
        );
    }
    #[test]
    fn the_entry_the_text_is_under_is_shown_in_reverse() {
        let open = OpenOutline::new(tree(&[(0, "Plans"), (0, "Rex")]));
        let lines = open.lines(Some(1), 6, "None", |_, style, _| {
            vec![Span::styled("x", style)]
        });
        let reversed = |line: &Line| {
            line.spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        };
        assert!(!reversed(&lines[1]));
        assert!(reversed(&lines[2]));
    }

    #[test]
    fn the_same_entries_on_another_page_are_a_new_tree() {
        let mut outline = tree(&[(0, "A"), (1, "B")]);
        outline.fold(0, true);
        outline.filter.push('a');
        assert!(outline.retarget(entries(&[(0, "A"), (1, "B")]), true));
        assert_eq!(outline.filter, "a");
        outline.filter.clear();
        assert!(!outline.folded(0));
    }

    #[test]
    fn what_was_followed_and_pinned_belongs_to_the_tree_before() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let mut open = OpenOutline::new(tree(&[(0, "A"), (1, "B")]));
        open.follows(Some(1));
        open.pinned = Some(Pinned {
            node: 1,
            landed: (2, 3),
        });
        assert!(open.retarget(entries(&[(0, "A"), (0, "B")]), false));
        assert_eq!((open.followed, open.pinned.is_none()), (None, true));
        // Typing across a new tree picks that tree's best match.
        open.key(&key(KeyCode::Char('/')));
        open.key(&key(KeyCode::Char('b')));
        assert_eq!(open.picked, Some(1));
        assert!(open.retarget(entries(&[(0, "Bee"), (1, "Cee")]), true));
        assert_eq!(open.picked, Some(0));
        assert_eq!(open.tree.filter, "b");
    }

    #[test]
    fn the_key_line_fits_the_panel_beside_the_text() {
        let open = OpenOutline::new(tree(&[(0, "A")]));
        let line = open.lines(None, 6, "None", |_, _, _| vec![]).pop().unwrap();
        assert!(line.width() <= 30);
    }

    #[test]
    fn unfolding_opens_the_group_that_hides_the_entry_before_what_it_folds() {
        let mut outline = tree(&[
            (0, "leaf"),
            (1, "Configuration"),
            (1, "Margin"),
            (2, "Plans"),
            (1, "Rex"),
        ]);
        outline.fold_from(3, true);
        outline.fold_from(3, true);
        assert_eq!(labels(&outline, &outline.visible()), ["leaf"]);
        outline.fold_from(3, false);
        assert_eq!(
            labels(&outline, &outline.visible()),
            ["leaf", "Configuration", "Margin", "Rex"]
        );
        assert!(
            outline.folded(2),
            "Margin stays folded until it is asked by itself"
        );
        outline.fold_from(3, false);
        assert_eq!(outline.visible(), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn slash_alone_keeps_the_entry_under_the_text_marked() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let mut open = OpenOutline::new(tree(&[(0, "A"), (1, "B"), (0, "C")]));
        open.follows(Some(1));
        open.key(&key(KeyCode::Char('/')));
        let lines = open.lines(Some(1), 6, "None", |entry, style, _| {
            vec![Span::styled(entry.label.to_owned(), style)]
        });
        assert_eq!(lines[2].to_string(), "›   B");
        assert!(
            lines[2]
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        );
    }

    #[test]
    fn an_open_outline_takes_its_own_keys_and_leaves_the_rest() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let mut open = OpenOutline::new(tree(&[(0, "A"), (1, "B"), (0, "C")]));
        assert_eq!(open.key(&key(KeyCode::Char('/'))), (true, None));
        // While the filter is typed every key is the outline's, letters among
        // them: a navigation key is a character to find.
        assert_eq!(open.key(&key(KeyCode::Char('c'))), (true, None));
        assert_eq!(open.tree.visible(), vec![2]);
        assert_eq!(open.key(&key(KeyCode::Backspace)), (true, None));
        assert_eq!(open.tree.visible(), vec![0, 1, 2]);
        assert_eq!(open.key(&key(KeyCode::Esc)), (true, None));
        assert_eq!(open.key(&key(KeyCode::Char('x'))), (false, None));
        assert_eq!(open.key(&key(KeyCode::Char('q'))), (true, Some(Ask::Close)));
        open.follows(Some(1));
        assert_eq!(open.key(&key(KeyCode::Char('h'))), (true, None));
        assert_eq!(open.tree.visible(), vec![0, 2]);
        assert_eq!(open.key(&key(KeyCode::Right)), (true, None));
        assert_eq!(open.tree.visible(), vec![0, 1, 2]);
        assert_eq!(open.key(&key(KeyCode::Up)), (true, Some(Ask::Go(Some(0)))));
        assert_eq!(open.key(&key(KeyCode::End)), (true, Some(Ask::Go(Some(2)))));
    }

    #[test]
    fn enter_goes_to_the_best_match_and_ends_the_filter() {
        let key = |code: KeyCode| KeyEvent::new(code, KeyModifiers::NONE);
        let mut open = OpenOutline::new(tree(&[
            (0, "Pull requests and issues"),
            (0, "Plans"),
            (0, "Rex"),
        ]));
        open.key(&key(KeyCode::Char('/')));
        for c in "pla".chars() {
            open.key(&key(KeyCode::Char(c)));
        }
        // A close match is picked over an earlier, scattered one.
        assert_eq!(open.picked, Some(1));
        assert_eq!(
            open.key(&key(KeyCode::Enter)),
            (true, Some(Ask::Go(Some(1))))
        );
        assert!(!open.typing);
        assert_eq!(open.tree.filter, "");
        // Escape ends the search and goes nowhere.
        for code in ['/', 'r', 'e', 'x'] {
            open.key(&key(KeyCode::Char(code)));
        }
        assert_eq!(open.picked, Some(2));
        assert_eq!(open.key(&key(KeyCode::Esc)), (true, None));
        assert_eq!(open.tree.filter, "");
        assert_eq!(open.picked, None);
        // With no match, Enter has nowhere to go.
        open.key(&key(KeyCode::Char('/')));
        open.key(&key(KeyCode::Char('z')));
        assert_eq!(open.key(&key(KeyCode::Enter)), (true, Some(Ask::Go(None))));
        assert!(!open.typing);
    }

    #[test]
    fn tab_and_the_arrows_move_between_the_matches() {
        let key = |code: KeyCode, modifiers: KeyModifiers| KeyEvent::new(code, modifiers);
        let mut open = OpenOutline::new(tree(&[(0, "One"), (1, "Two"), (0, "Three")]));
        open.key(&key(KeyCode::Char('/'), KeyModifiers::NONE));
        open.key(&key(KeyCode::Char('t'), KeyModifiers::NONE));
        // Of two equally close matches, the earlier is picked.
        assert_eq!(open.picked, Some(1));
        open.key(&key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(open.picked, Some(2));
        // Past the last match the pick stays put.
        open.key(&key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(open.picked, Some(2));
        open.key(&key(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert_eq!(open.picked, Some(1));
        open.key(&key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(open.picked, Some(1));
        // A change to the query picks the best match afresh.
        open.key(&key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(open.picked, Some(0));
    }

    #[test]
    fn the_outline_shows_the_entry_the_text_moves_under() {
        let mut open = OpenOutline::new(tree(&[(0, "A"), (1, "B"), (0, "C")]));
        open.tree.fold(0, true);
        open.follows(Some(0));
        assert_eq!(open.tree.visible(), vec![0, 2]);
        // On to what the fold hid: it comes back.
        open.follows(Some(1));
        assert_eq!(open.tree.visible(), vec![0, 1, 2]);
    }

    #[test]
    fn a_hidden_entry_is_shown_by_its_group() {
        let mut outline = tree(&[(0, "A"), (1, "B"), (2, "C"), (0, "D")]);
        outline.fold(0, true);
        assert_eq!(outline.shown(Some(0)), Some(0));
        assert_eq!(outline.shown(Some(2)), Some(0));
        assert_eq!(outline.shown(Some(3)), Some(3));
        // A filter keeps what it matches, and the entries above them.
        outline.fold(0, false);
        outline.filter.push('c');
        assert_eq!(outline.shown(Some(2)), Some(2));
        assert_eq!(outline.shown(Some(3)), None);
        assert_eq!(outline.shown(None), None);
    }

    #[test]
    fn the_heading_a_row_sits_under_is_the_last_one_that_starts_above_it() {
        let headings = [
            heading(0, 1, "One", 1),
            heading(1, 2, "Two", 5),
            heading(2, 1, "Three", 9),
        ];
        assert_eq!(heading_at(&headings, 0), Some(0));
        assert_eq!(heading_at(&headings, 1), Some(0));
        assert_eq!(heading_at(&headings, 6), Some(1));
        assert_eq!(heading_at(&headings, 9), Some(2));
        assert_eq!(heading_at(&headings, 400), Some(2));
        assert_eq!(heading_at(&[], 3), None);
    }

    #[test]
    fn headings_nest_by_level_and_hold_the_renderer_s_numbers() {
        let told = [
            heading(0, 1, "One", 1),
            heading(1, 2, "Two", 4),
            heading(2, 3, "Three", 7),
            heading(3, 2, "Four", 10),
            heading(4, 1, "Five", 13),
        ];
        let nodes = from_headings(&told);
        let got: Vec<(&str, usize, Option<Locator>)> = nodes
            .iter()
            .map(|node| (node.label.as_str(), node.depth, node.locator))
            .collect();
        assert_eq!(
            got,
            [
                ("One", 0, Some(Locator::Text(0))),
                ("Two", 1, Some(Locator::Text(1))),
                ("Three", 2, Some(Locator::Text(2))),
                // Back up to the shallower heading, not the one just above.
                ("Four", 1, Some(Locator::Text(3))),
                ("Five", 0, Some(Locator::Text(4))),
            ]
        );
    }

    #[test]
    fn a_page_that_opens_deep_starts_at_the_root() {
        let told = [
            heading(0, 3, "Deep", 1),
            heading(1, 4, "Deeper", 4),
            heading(2, 2, "Up", 7),
        ];
        let nodes = from_headings(&told);
        let got: Vec<(&str, usize)> = nodes
            .iter()
            .map(|node| (node.label.as_str(), node.depth))
            .collect();
        assert_eq!(got, [("Deep", 0), ("Deeper", 1), ("Up", 0)]);
    }

    fn bookmark(title: &str, depth: usize, page: usize) -> crate::doc::Bookmark {
        crate::doc::Bookmark {
            title: title.to_owned(),
            depth,
            page,
        }
    }

    #[test]
    fn bookmarks_keep_the_depth_they_came_with() {
        let nodes = from_bookmarks(&[bookmark("One", 0, 0), bookmark("Nested", 1, 2)]);
        assert_eq!(nodes[0].depth, 0);
        assert_eq!(nodes[1].depth, 1);
    }

    #[test]
    fn a_page_belongs_to_the_last_bookmark_at_or_before_it() {
        let bookmarks = [bookmark("One", 0, 2), bookmark("Two", 0, 5)];
        // Pages before the first bookmark belong to it.
        assert_eq!(page_at(&bookmarks, 0), Some(0));
        assert_eq!(page_at(&bookmarks, 2), Some(0));
        assert_eq!(page_at(&bookmarks, 4), Some(0));
        assert_eq!(page_at(&bookmarks, 5), Some(1));
        assert_eq!(page_at(&bookmarks, 40), Some(1));
        assert_eq!(page_at(&[], 0), None);
    }

    #[test]
    fn a_page_belongs_to_the_bookmark_at_or_before_it_that_goes_furthest() {
        // Bookmarks need not come in page order, so the nearest one is not the
        // last one listed.
        let bookmarks = [bookmark("Late", 0, 5), bookmark("Early", 0, 2)];
        // A page before them all belongs to the first entry, as ever.
        assert_eq!(page_at(&bookmarks, 1), Some(0));
        assert_eq!(page_at(&bookmarks, 2), Some(1));
        assert_eq!(page_at(&bookmarks, 4), Some(1));
        assert_eq!(page_at(&bookmarks, 5), Some(0));
        assert_eq!(page_at(&bookmarks, 9), Some(0));
        // Two on one page: the earlier of them holds it.
        let shared = [bookmark("One", 0, 3), bookmark("Two", 0, 3)];
        assert_eq!(page_at(&shared, 3), Some(0));
    }

    #[test]
    fn a_heading_with_no_words_holds_no_entry_and_no_page() {
        let told = [
            heading(0, 2, "", 1),
            heading(1, 2, "Two", 5),
            heading(2, 2, "", 9),
        ];
        let nodes = from_headings(&told);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].locator, Some(Locator::Text(1)));
        // The text under an untitled heading belongs to the titled one before.
        assert_eq!(heading_at(&told, 0), Some(1));
        assert_eq!(heading_at(&told, 4), Some(1));
        assert_eq!(heading_at(&told, 9), Some(1));
        assert_eq!(heading_at(&told, 20), Some(1));
    }

    fn file(old: Option<&str>, new: Option<&str>) -> diff::File {
        diff::File {
            old_path: old.map(str::to_owned),
            new_path: new.map(str::to_owned),
            header_row: 0,
            hunks: Vec::new(),
            added: 0,
            removed: 0,
            binary: false,
            note: None,
            identity: String::new(),
        }
    }

    fn changed(pairs: &[&str]) -> Vec<diff::File> {
        pairs
            .iter()
            .map(|path| file(Some(path), Some(path)))
            .collect()
    }
    /// (label, depth, locator), as the panel lists them.
    fn shape(files: &[diff::File]) -> Vec<(String, usize, Option<Locator>)> {
        from_diff_files(files)
            .into_iter()
            .map(|node| (node.label, node.depth, node.locator))
            .collect()
    }

    #[test]
    fn diff_files_hang_under_the_directories_their_paths_lead_through() {
        let files = changed(&["src/core/a.rs", "src/core/b.rs", "src/main.rs"]);
        assert_eq!(
            shape(&files),
            [
                ("src".to_owned(), 0, None),
                ("core".to_owned(), 1, None),
                ("a.rs".to_owned(), 2, Some(Locator::Diff(0))),
                ("b.rs".to_owned(), 2, Some(Locator::Diff(1))),
                // The next file goes back up to the shallower directory.
                ("main.rs".to_owned(), 1, Some(Locator::Diff(2))),
            ]
        );
        // A directory met again is the entry it was, and an absolute path hangs from
        // a root with no name.
        let files = changed(&["src/a.rs", "docs/b.rs", "src/c.rs", "/src/d.rs"]);
        let nodes = from_diff_files(&files);
        let got: Vec<(&str, usize)> = nodes
            .iter()
            .map(|node| (node.label.as_str(), node.depth))
            .collect();
        assert_eq!(
            got,
            [
                ("src", 0),
                ("a.rs", 1),
                ("c.rs", 1),
                ("docs", 0),
                ("b.rs", 1),
                ("", 0),
                ("src", 1),
                ("d.rs", 2),
            ]
        );
    }
}
