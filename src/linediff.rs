//! What changed between a removed line and the added line it is paired with,
//! and which lines of a change block pair up. Both follow Pierre's
//! `@pierre/diffs`: emphasis is its default `word-alt` line diff, and pairing
//! is its similarity realignment of blocks with more lines on one side.

use std::ops::Range;

use similar::{Algorithm, DiffTag, capture_diff_slices};

/// Lines longer than this, in characters, are not compared word by word; the
/// change between their shared start and end is emphasized instead. Pierre's
/// default `maxLineDiffLength`.
const MAX_WORD_DIFF: usize = 1000;

/// A block needing more line comparisons than this keeps positional pairing.
const MAX_ALIGNMENT_COMPARISONS: usize = 4096;

/// How much better per pair a shifted pairing must score to replace the
/// positional one, so near ties keep Git's order.
const MIN_IMPROVEMENT_PER_PAIR: f64 = 0.5;

/// The changed text of a removed line and of the added line paired with it,
/// as character ranges into each.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub old: Vec<Range<usize>>,
    pub new: Vec<Range<usize>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Same,
    Old,
    New,
}

impl Changes {
    /// Compares `old` with `new` by word, joining changes that only a single
    /// unchanged character separates, as Pierre's `word-alt` does.
    pub fn between(old: &str, new: &str) -> Self {
        let (old_len, new_len) = (old.chars().count(), new.chars().count());
        if old_len > MAX_WORD_DIFF || new_len > MAX_WORD_DIFF {
            return Self::outside_shared_ends(old, new, old_len, new_len);
        }
        let (old_tokens, new_tokens) = (tokens(old), tokens(new));
        let chars = |tokens: &[&str], range: Range<usize>| {
            tokens[range]
                .iter()
                .map(|t| t.chars().count())
                .sum::<usize>()
        };
        let mut parts: Vec<(Part, usize)> = Vec::new();
        for op in capture_diff_slices(Algorithm::Myers, &old_tokens, &new_tokens) {
            let (tag, old_range, new_range) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => parts.push((Part::Same, chars(&old_tokens, old_range))),
                DiffTag::Delete => parts.push((Part::Old, chars(&old_tokens, old_range))),
                DiffTag::Insert => parts.push((Part::New, chars(&new_tokens, new_range))),
                DiffTag::Replace => {
                    parts.push((Part::Old, chars(&old_tokens, old_range)));
                    parts.push((Part::New, chars(&new_tokens, new_range)));
                }
            }
        }
        Changes {
            old: side(&parts, Part::Old),
            new: side(&parts, Part::New),
        }
    }

    /// One range per side, between the characters both lines start and end with.
    fn outside_shared_ends(old: &str, new: &str, old_len: usize, new_len: usize) -> Self {
        let prefix = old
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = old
            .chars()
            .rev()
            .zip(new.chars().rev())
            .take_while(|(a, b)| a == b)
            .count()
            .min(old_len - prefix)
            .min(new_len - prefix);
        let range = |len: usize| (prefix < len - suffix).then_some(prefix..len - suffix);
        Changes {
            old: range(old_len).into_iter().collect(),
            new: range(new_len).into_iter().collect(),
        }
    }
}

/// Runs of letters, digits and underscores, runs of whitespace, and each
/// other character alone, as jsdiff's `diffWordsWithSpace` splits a line.
fn tokens(text: &str) -> Vec<&str> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut previous = None;
    for (at, c) in text.char_indices() {
        let current = class(c);
        if at > start && (Some(&current) != previous.as_ref() || current == Class::Other) {
            tokens.push(&text[start..at]);
            start = at;
        }
        previous = Some(current);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

/// The changed ranges of one side. An unchanged single character right after
/// a change joins it, unless it ends the line, so `old value` against
/// `new thing` is one change rather than two with a gap.
fn side(parts: &[(Part, usize)], changed: Part) -> Vec<Range<usize>> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    let last = parts.len().saturating_sub(1);
    for (index, &(part, len)) in parts.iter().enumerate() {
        if part != Part::Same && part != changed {
            continue;
        }
        let is_change = part == changed;
        match runs.last_mut() {
            Some(run)
                if index != last && (run.0 == is_change || (!is_change && len == 1 && run.0)) =>
            {
                run.1 += len;
            }
            _ => runs.push((is_change, len)),
        }
    }
    let mut at = 0;
    let mut ranges = Vec::new();
    for (is_change, len) in runs {
        if is_change && len > 0 {
            ranges.push(at..at + len);
        }
        at += len;
    }
    ranges
}

/// How far to shift the shorter side of a change block along the longer one
/// so its lines pair with the lines most like them. `0` keeps positional
/// pairing. With more added lines, added line `offset + i` pairs with
/// removed line `i`; with more removed lines, the other way round.
pub fn alignment(old: &[&str], new: &[&str]) -> usize {
    let pairs = old.len().min(new.len());
    let surplus = old.len().abs_diff(new.len());
    if pairs == 0 || surplus == 0 || pairs.saturating_mul(surplus + 1) > MAX_ALIGNMENT_COMPARISONS {
        return 0;
    }
    let strip = |lines: &[&str]| -> Vec<Vec<char>> {
        lines
            .iter()
            .map(|line| line.chars().filter(|c| !c.is_whitespace()).collect())
            .collect()
    };
    let (old, new) = (strip(old), strip(new));
    let new_longer = new.len() > old.len();
    let score = |offset: usize| -> f64 {
        (0..pairs)
            .map(|pair| {
                let (o, n) = if new_longer {
                    (pair, pair + offset)
                } else {
                    (pair + offset, pair)
                };
                similarity(&old[o], &new[n])
            })
            .sum()
    };
    let mut best = score(0) + pairs as f64 * MIN_IMPROVEMENT_PER_PAIR;
    let mut offset = 0;
    for shift in 1..=surplus {
        let s = score(shift);
        if s > best {
            best = s;
            offset = shift;
        }
    }
    offset
}

/// The shared start and end of two lines over the longer length: 1 for equal
/// lines, 0 when either is blank.
fn similarity(a: &[char], b: &[char]) -> f64 {
    if a == b {
        return 1.0;
    }
    let shorter = a.len().min(b.len());
    if shorter == 0 {
        return 0.0;
    }
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(shorter - prefix)
        .take_while(|(x, y)| x == y)
        .count();
    (prefix + suffix) as f64 / a.len().max(b.len()) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marked(text: &str, ranges: &[Range<usize>]) -> String {
        let mut out = String::new();
        for (at, c) in text.chars().enumerate() {
            if ranges.iter().any(|r| r.start == at) {
                out.push('[');
            }
            out.push(c);
            if ranges.iter().any(|r| r.end == at + 1) {
                out.push(']');
            }
        }
        out
    }

    fn both(old: &str, new: &str) -> (String, String) {
        let changes = Changes::between(old, new);
        (marked(old, &changes.old), marked(new, &changes.new))
    }

    #[test]
    fn marks_each_changed_word_not_the_span_between() {
        assert_eq!(
            both("foo(a, b, c)", "foo(x, b, y)"),
            ("foo([a], b, [c])".into(), "foo([x], b, [y])".into())
        );
        assert_eq!(
            both("let retries = 3;", "let retries = 5;"),
            ("let retries = [3];".into(), "let retries = [5];".into())
        );
    }

    #[test]
    fn joins_changes_one_character_apart_like_word_alt() {
        assert_eq!(
            both("old value here", "new thing here"),
            ("[old value] here".into(), "[new thing] here".into())
        );
        assert_eq!(
            both("a.b + 1", "x.y + 1"),
            ("[a.b] + 1".into(), "[x.y] + 1".into())
        );
        // Two unchanged characters keep changes apart.
        assert_eq!(both("a, b", "x, y"), ("[a], [b]".into(), "[x], [y]".into()));
        // An unchanged character that ends the line is never joined.
        assert_eq!(both("f(a)", "f(b)"), ("f([a])".into(), "f([b])".into()));
    }

    #[test]
    fn insertions_mark_only_the_new_side() {
        assert_eq!(
            both("call(a)", "call(a, b)"),
            ("call(a)".into(), "call(a[, b])".into())
        );
        assert_eq!(both("same", "same"), ("same".into(), "same".into()));
    }

    #[test]
    fn counts_characters_not_bytes() {
        assert_eq!(
            both("let café = 1;", "let thé = 1;"),
            ("let [café] = 1;".into(), "let [thé] = 1;".into())
        );
        assert_eq!(
            both("emoji 👩‍💻 here", "emoji 👩‍💻 there"),
            ("emoji 👩‍💻 [here]".into(), "emoji 👩‍💻 [there]".into())
        );
    }

    #[test]
    fn long_lines_mark_between_shared_ends() {
        let old = format!("{} old {}", "a".repeat(MAX_WORD_DIFF), "z");
        let new = format!("{} new {}", "a".repeat(MAX_WORD_DIFF), "z");
        let changes = Changes::between(&old, &new);
        let at = MAX_WORD_DIFF + 1;
        assert_eq!(changes.old, vec![at..at + 3]);
        assert_eq!(changes.new, vec![at..at + 3]);
        assert_eq!(Changes::between(&old, &old), Changes::default());
    }

    #[test]
    fn an_inserted_line_above_an_edit_shifts_the_pairing() {
        // Pressing Enter above an edited line: Git pairs the blank with it.
        let old = ["    let total = sum(items);"];
        let new = ["", "    let total = sum(items) + tax;"];
        assert_eq!(alignment(&old, &new), 1);
        let old = ["    let total = sum(items);", "    log(total);"];
        let new = ["    let total = sum(items) + tax;"];
        assert_eq!(alignment(&old, &new), 0);
        let old = ["    debug();", "    let total = sum(items);"];
        assert_eq!(alignment(&old, &new), 1);
    }

    #[test]
    fn near_ties_and_balanced_blocks_keep_git_order() {
        let old = ["import { a } from 'a';"];
        let new = ["import { b } from 'b';", "import { c } from 'c';"];
        assert_eq!(alignment(&old, &new), 0);
        assert_eq!(alignment(&["a", "b"], &["c", "d"]), 0);
        assert_eq!(alignment(&[], &["c", "d"]), 0);
    }
}
