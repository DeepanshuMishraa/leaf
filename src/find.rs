//! Finding text: where a query occurs in a page's text.

/// What the reader is looking for. Matching ignores case, and a space in the
/// query matches any run of whitespace, so a phrase is found across a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    typed: String,
    folded: Vec<char>,
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

impl Query {
    /// `None` for a query of nothing but whitespace.
    pub fn new(typed: &str) -> Option<Query> {
        let words: Vec<String> = typed
            .split_whitespace()
            .map(|w| w.chars().map(fold).collect())
            .collect();
        if words.is_empty() {
            return None;
        }
        Some(Query {
            typed: typed.trim().to_owned(),
            folded: words.join(" ").chars().collect(),
        })
    }

    /// The query as typed, trimmed.
    pub fn typed(&self) -> &str {
        &self.typed
    }

    /// Where a match starting at `start` ends, if one does.
    fn match_at(&self, text: &[char], start: usize) -> Option<usize> {
        let space = |i: usize| text.get(i).is_some_and(|c| c.is_whitespace());
        let mut i = start;
        for &q in &self.folded {
            if q == ' ' {
                if !space(i) {
                    return None;
                }
                while space(i) {
                    i += 1;
                }
            } else if text.get(i).map(|&c| fold(c)) == Some(q) {
                i += 1;
            } else {
                return None;
            }
        }
        Some(i)
    }

    /// Every place the query occurs in `text`, as character ranges, first to last, not overlapping.
    pub fn matches(&self, text: &[char]) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let mut start = 0;
        while start < text.len() {
            match self.match_at(text, start) {
                Some(end) => {
                    found.push((start, end));
                    start = end;
                }
                None => start += 1,
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn found(query: &str, text: &str) -> Vec<(usize, usize)> {
        Query::new(query)
            .expect("query has text")
            .matches(&chars(text))
    }

    #[test]
    fn a_blank_query_is_none() {
        assert_eq!(Query::new("  \t "), None);
    }

    #[test]
    fn matching_ignores_case_and_finds_every_occurrence() {
        assert_eq!(
            found("raft", "Raft and RAFT, not rafters? raft"),
            vec![(0, 4), (9, 13), (19, 23), (28, 32)]
        );
    }

    #[test]
    fn a_space_matches_a_line_break_or_several_spaces() {
        assert_eq!(
            found("total  order", "Total\norder and total   order"),
            vec![(0, 11), (16, 29)]
        );
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(found("aa", "aaaa a"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn nothing_matches_a_query_longer_than_the_text() {
        assert_eq!(found("consensus", "consen"), vec![]);
    }

    #[test]
    fn the_typed_query_is_kept_trimmed() {
        assert_eq!(
            Query::new("  Leader Election ").map(|q| q.typed().to_owned()),
            Some("Leader Election".to_owned())
        );
    }
}
