//! Finding a query inside a computed diff.
//!
//! Pure: it takes an alignment plus the two texts and returns positions. Nothing here
//! knows about the terminal, which is what makes it straightforward to test.

use crate::model::{Alignment, SideIdx};
use crate::text::TextFile;

/// One hit: a byte range inside one side's line on one aligned row.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    pub row: u32,
    pub side: SideIdx,
    pub start: u32,
    pub end: u32,
}

/// Search every row of `alignment`, in display order.
///
/// Matching is **smart-case**: a query with no uppercase matches case-insensitively, one
/// with any uppercase matches exactly. That is the `rg`/vim convention and needs no flag.
///
/// Case folding is ASCII-only. A non-ASCII query always matches exactly, which keeps byte
/// offsets pointing at the real text — Unicode case folding can change a string's length,
/// and an offset into a folded copy would not survive the trip back.
pub fn find(alignment: &Alignment, texts: [&TextFile; 2], query: &str) -> Vec<Match> {
    if query.is_empty() {
        return Vec::new();
    }
    let ignore_case = !query.bytes().any(|b| b.is_ascii_uppercase());
    let mut out = Vec::new();

    for (row, aligned) in alignment.rows().iter().enumerate() {
        for (index, side) in [SideIdx::LEFT, SideIdx::RIGHT].into_iter().enumerate() {
            let Some(line_no) = aligned.cells[index].line() else {
                continue;
            };
            let line = texts[index].line(line_no);
            out.extend(
                find_in_line(line, query, ignore_case).map(|(start, end)| Match {
                    row: row as u32,
                    side,
                    start,
                    end,
                }),
            );
        }
    }
    out
}

/// Non-overlapping occurrences of `query` in `line`, as byte ranges.
///
/// Byte-wise on purpose. UTF-8 is self-synchronising, so a valid needle can only ever
/// match at a character boundary — no boundary check is needed, and the offsets returned
/// index the original line directly.
fn find_in_line<'a>(
    line: &'a str,
    query: &'a str,
    ignore_case: bool,
) -> impl Iterator<Item = (u32, u32)> + 'a {
    let (haystack, needle) = (line.as_bytes(), query.as_bytes());
    let mut at = 0usize;
    std::iter::from_fn(move || {
        while at + needle.len() <= haystack.len() {
            let window = &haystack[at..at + needle.len()];
            let hit = if ignore_case {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if hit {
                let range = (at as u32, (at + needle.len()) as u32);
                at += needle.len();
                return Some(range);
            }
            at += 1;
        }
        None
    })
}

/// Index of the first match at or after `row`, wrapping to the start.
pub fn next_from(matches: &[Match], row: u32, forward: bool) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    if forward {
        matches
            .iter()
            .position(|m| m.row >= row)
            .or(Some(0))
            .map(|i| i.min(matches.len() - 1))
    } else {
        matches
            .iter()
            .rposition(|m| m.row <= row)
            .or(Some(matches.len() - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffOptions, align_text};
    use crate::text::decode;
    use std::path::Path;

    fn file(body: &str) -> TextFile {
        decode(Path::new("t"), body.as_bytes().to_vec(), None)
    }

    /// Search a diff and render the hits as `row:side:text`.
    fn hits(left: &str, right: &str, query: &str) -> Vec<String> {
        let (l, r) = (file(left), file(right));
        let alignment = align_text(&l, &r, &DiffOptions::default());
        find(&alignment, [&l, &r], query)
            .into_iter()
            .map(|m| {
                let text = if m.side == SideIdx::LEFT { &l } else { &r };
                let line_no = alignment.cell(m.row as usize, m.side).line().unwrap();
                let slice = &text.line(line_no)[m.start as usize..m.end as usize];
                format!(
                    "{}:{}:{slice}",
                    m.row,
                    if m.side == SideIdx::LEFT { "L" } else { "R" }
                )
            })
            .collect()
    }

    #[test]
    fn a_match_is_found_on_both_sides_of_an_equal_row() {
        assert_eq!(hits("alpha\n", "alpha\n", "alp"), ["0:L:alp", "0:R:alp"]);
    }

    #[test]
    fn matches_are_reported_in_display_order() {
        let out = hits("one\ntwo\nthree\n", "one\ntwo\nthree\n", "o");
        // Row 0 both sides, then row 1 both sides. Left before right within a row.
        assert_eq!(out, ["0:L:o", "0:R:o", "1:L:o", "1:R:o"]);
    }

    #[test]
    fn a_lowercase_query_ignores_case() {
        assert_eq!(
            hits("Hello\n", "HELLO\n", "hello"),
            ["0:L:Hello", "0:R:HELLO"]
        );
    }

    #[test]
    fn an_uppercase_query_matches_exactly() {
        // Smart case: the capital makes the search precise.
        assert_eq!(
            hits("Hello hello\n", "Hello hello\n", "Hello"),
            ["0:L:Hello", "0:R:Hello"]
        );
    }

    #[test]
    fn overlapping_occurrences_are_not_double_counted() {
        // "aaaa" contains "aa" twice without overlap, not three times.
        assert_eq!(hits("aaaa\n", "aaaa\n", "aa").len(), 4);
    }

    #[test]
    fn offsets_index_the_original_line_through_multibyte_text() {
        let out = hits("héllo wörld\n", "héllo wörld\n", "wörld");
        assert_eq!(out, ["0:L:wörld", "0:R:wörld"]);
    }

    #[test]
    fn a_filler_side_is_skipped() {
        // The right side has no line on the deleted row, so only the left can match.
        let out = hits("keep\ndrop\n", "keep\n", "drop");
        assert_eq!(out, ["1:L:drop"]);
    }

    #[test]
    fn an_empty_query_finds_nothing() {
        assert!(hits("a\n", "a\n", "").is_empty());
    }

    #[test]
    fn a_query_longer_than_the_line_finds_nothing() {
        assert!(hits("ab\n", "ab\n", "abcdef").is_empty());
    }

    #[test]
    fn navigation_wraps_in_both_directions() {
        let m = |row| Match {
            row,
            side: SideIdx::LEFT,
            start: 0,
            end: 1,
        };
        let matches = [m(2), m(5), m(9)];

        assert_eq!(next_from(&matches, 0, true), Some(0));
        assert_eq!(next_from(&matches, 3, true), Some(1));
        // Past the last match, wrap to the first.
        assert_eq!(next_from(&matches, 99, true), Some(0));

        assert_eq!(next_from(&matches, 9, false), Some(2));
        assert_eq!(next_from(&matches, 4, false), Some(0));
        // Before the first match, wrap round to the last.
        assert_eq!(next_from(&matches, 0, false), Some(2));
        assert_eq!(next_from(&[], 0, true), None);
    }
}
