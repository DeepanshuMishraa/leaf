//! Finding text: where a query occurs in a page's text.

/// What the reader is looking for. Matching ignores case and the difference
/// between typed and typeset punctuation, and a space in the query matches any
/// run of whitespace, so a phrase is found across a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    typed: String,
    folded: Vec<char>,
}

/// One character for each, so a match's range in the folded text is its range in the page.
/// Curly quotes and Unicode hyphens fold to what a keyboard types: PDFs and
/// articles set "don’t" and "state‐of‐the‐art", and readers search for "don't".
fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{02BC}' | '\u{2032}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => '"',
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2212}' => '-',
        c => c.to_lowercase().next().unwrap_or(c),
    }
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

    #[test]
    fn a_typed_apostrophe_finds_a_curly_one() {
        assert_eq!(
            found("don't", "I don\u{2019}t know, I don't"),
            vec![(2, 7), (16, 21)]
        );
    }

    #[test]
    fn a_curly_apostrophe_in_the_query_finds_a_typed_one() {
        assert_eq!(found("don\u{2019}t", "I don't know"), vec![(2, 7)]);
    }

    #[test]
    fn typed_quotes_find_curly_quotes() {
        assert_eq!(
            found("\"total order\"", "a \u{201C}total order\u{201D} holds"),
            vec![(2, 15)]
        );
    }

    #[test]
    fn typed_quotes_find_low_quotes_and_the_modifier_apostrophe() {
        assert_eq!(
            found("\"zitat\"", "ein \u{201E}Zitat\u{201C} hier"),
            vec![(4, 11)]
        );
        assert_eq!(found("don't", "don\u{02BC}t"), vec![(0, 5)]);
    }

    #[test]
    fn a_typed_hyphen_finds_unicode_hyphens_and_dashes() {
        assert_eq!(
            found("state-of-the-art", "state\u{2010}of\u{2011}the\u{2010}art"),
            vec![(0, 16)]
        );
        assert_eq!(found("pages 3-9", "pages 3\u{2013}9"), vec![(0, 9)]);
    }

    #[test]
    fn an_em_dash_is_not_a_hyphen() {
        assert_eq!(found("well-known", "well\u{2014}known"), vec![]);
    }
}
