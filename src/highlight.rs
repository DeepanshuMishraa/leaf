//! Highlighted stretches of page text.

use crate::hue::Hue;

/// A highlighted stretch of a page. `page` is 0-based (0 for documents
/// without pages); `start` and `end` are character offsets into the page's
/// text as the viewer lays it out. `text` is what was highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub page: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub hue: Hue,
}

fn overlaps(a: &Highlight, b: &Highlight) -> bool {
    a.page == b.page && a.start < b.end && b.start < a.end
}

/// Joins ranges on the same page that meet across a line break, so one
/// selection makes one highlight per page.
pub fn merge(mut ranges: Vec<Highlight>) -> Vec<Highlight> {
    ranges.sort_by_key(|r| (r.page, r.start));
    let mut merged: Vec<Highlight> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if last.page == range.page && range.start <= last.end + 1 => {
                last.end = last.end.max(range.end);
                last.text = format!("{}\n{}", last.text, range.text);
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// Highlights the selection, or, when it touches highlights already there,
/// clears those instead.
pub fn toggle(existing: &[Highlight], selection: &[Highlight]) -> Vec<Highlight> {
    let touched = |h: &Highlight| selection.iter().any(|r| overlaps(h, r));
    if existing.iter().any(touched) {
        return existing.iter().filter(|h| !touched(h)).cloned().collect();
    }
    existing
        .iter()
        .cloned()
        .chain(selection.iter().filter(|r| r.end > r.start).cloned())
        .collect()
}

/// Turns the highlights the selection touches `hue`, or, when it touches
/// none, highlights the selection in `hue`.
pub fn paint(existing: &[Highlight], selection: &[Highlight], hue: Hue) -> Vec<Highlight> {
    let touched = |h: &Highlight| selection.iter().any(|r| overlaps(h, r));
    if !existing.iter().any(touched) {
        let fresh: Vec<Highlight> = selection
            .iter()
            .map(|r| Highlight { hue, ..r.clone() })
            .collect();
        return toggle(existing, &fresh);
    }
    existing
        .iter()
        .map(|h| {
            if touched(h) {
                Highlight { hue, ..h.clone() }
            } else {
                h.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(page: usize, start: usize, end: usize) -> Highlight {
        Highlight {
            page,
            start,
            end,
            text: format!("{start}-{end}"),
            hue: Hue::Yellow,
        }
    }

    #[test]
    fn painting_recolors_what_the_selection_touches_or_adds_it() {
        let existing = vec![h(0, 0, 4), h(0, 10, 14)];
        let painted = paint(&existing, &[h(0, 2, 3)], Hue::Blue);
        assert_eq!(painted[0].hue, Hue::Blue);
        assert_eq!((painted[0].start, painted[0].end), (0, 4));
        assert_eq!(painted[1].hue, Hue::Yellow);
        let added = paint(&existing, &[h(0, 5, 8)], Hue::Green);
        assert_eq!(added.len(), 3);
        assert_eq!(added[2].hue, Hue::Green);
    }

    #[test]
    fn ranges_meeting_across_a_break_merge() {
        let merged = merge(vec![h(0, 6, 9), h(0, 0, 5), h(1, 0, 2)]);
        assert_eq!(merged.len(), 2);
        assert_eq!((merged[0].start, merged[0].end), (0, 9));
        assert_eq!(merged[0].text, "0-5\n6-9");
    }

    #[test]
    fn toggling_adds_then_clears() {
        let added = toggle(&[], &[h(0, 2, 4)]);
        assert_eq!(added, vec![h(0, 2, 4)]);
        assert!(toggle(&added, &[h(0, 3, 5)]).is_empty());
        assert_eq!(toggle(&added, &[h(0, 4, 6)]).len(), 2);
    }
}
