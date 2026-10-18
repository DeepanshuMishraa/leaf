//! Highlighted stretches of page text.

/// A highlighted stretch of a page. `page` is 0-based (0 for documents
/// without pages); `start` and `end` are character offsets into the page's
/// text as the viewer lays it out. `text` is what was highlighted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Highlight {
    pub page: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn h(page: usize, start: usize, end: usize) -> Highlight {
        Highlight { page, start, end, text: format!("{start}-{end}") }
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
