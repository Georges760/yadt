//! Folding a `similar` op-stream into an [`Alignment`].
//!
//! This is the **only** module that names `similar`. Nothing downstream ever sees a
//! `DiffOp`, which is what keeps the diff algorithm swappable: replacing it means
//! rewriting [`align_text`] and nothing else.

use std::time::Duration;

use similar::{Algorithm, ChangeTag, DiffOp, TextDiff};

use crate::model::{Alignment, AlignmentBuilder, MAX_SIDES, RowKind, SideIdx};
use crate::text::{EolStyle, FileContent, TextFile, files_equal};

/// Default ceiling on the diff computation.
///
/// Myers is O(ND); two large, heavily-different files can otherwise stall the UI with no
/// way out. `similar` degrades to a coarser but still valid diff at the deadline.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub struct DiffOptions {
    pub algorithm: Algorithm,
    pub timeout: Duration,
    /// Compute word-level emphasis inside replace blocks.
    pub inline: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            // git's default, and for the same reason: it resists the "diff anchored on the
            // closing brace" pathology that Myers hits on code.
            algorithm: Algorithm::Histogram,
            timeout: DEFAULT_TIMEOUT,
            inline: true,
        }
    }
}

/// Align two text files.
pub fn align_text(left: &TextFile, right: &TextFile, opts: &DiffOptions) -> Alignment {
    let mut b = AlignmentBuilder::new(2);

    // Byte-identical content never reaches the diff algorithm. This is extremely common
    // when drilling in from the directory view.
    if left.text == right.text {
        b.push_equal(left.line_count() as u32);
        return b.finish();
    }

    // These must outlive `diff`, which borrows them.
    let lo = left.slices();
    let ln = right.slices();

    let diff = TextDiff::configure()
        .algorithm(opts.algorithm)
        .timeout(opts.timeout)
        // Lines are already stripped of their terminators, so inline byte offsets are
        // offsets into exactly the `&str` the renderer draws.
        .newline_terminated(false)
        .diff_slices(&lo, &ln);

    for op in diff.ops() {
        match *op {
            DiffOp::Equal { len, .. } => {
                b.push_equal(len as u32);
            }
            DiffOp::Delete { old_len, .. } => {
                b.push_block(RowKind::Unmatched, counts(old_len, 0));
            }
            DiffOp::Insert { new_len, .. } => {
                b.push_block(RowKind::Unmatched, counts(0, new_len));
            }
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                b.push_block(RowKind::Changed, counts(old_len, new_len));
                if opts.inline {
                    apply_inline(&mut b, &diff, op);
                }
            }
        }
    }

    b.finish()
}

fn counts(old_len: usize, new_len: usize) -> [u32; MAX_SIDES] {
    let mut c = [0u32; MAX_SIDES];
    c[SideIdx::LEFT.idx()] = old_len as u32;
    c[SideIdx::RIGHT.idx()] = new_len as u32;
    c
}

/// Attach word-level emphasis to the rows of one replace block.
///
/// Rows are located with `AlignmentBuilder::row_of` rather than by arithmetic on
/// `old_index + k`, so this stays correct whatever pairing policy the block used.
fn apply_inline(b: &mut AlignmentBuilder, diff: &TextDiff<'_, '_, str>, op: &DiffOp) {
    for change in diff.iter_inline_changes(op) {
        let (side, index) = match change.tag() {
            ChangeTag::Delete => (SideIdx::LEFT, change.old_index()),
            ChangeTag::Insert => (SideIdx::RIGHT, change.new_index()),
            ChangeTag::Equal => continue,
        };
        let Some(index) = index else { continue };
        let Some(row) = b.row_of(side, index as u32) else {
            continue;
        };

        // `values()` are the line's segments in order, so a running byte offset over them
        // is the offset into the line itself.
        let mut off = 0u32;
        let ranges: Vec<(u32, u32)> = change
            .values()
            .iter()
            .filter_map(|&(emphasized, value)| {
                let start = off;
                off += value.len() as u32;
                emphasized.then_some((start, off))
            })
            .collect();

        b.set_emphasis(row, side, ranges);
    }
}

/// The result of comparing one pair of paths.
#[derive(Debug)]
pub struct FileComparison {
    pub left: FileContent,
    pub right: FileContent,
    /// `None` when at least one side could not be line-diffed.
    pub alignment: Option<Alignment>,
    /// Banner text for anything the alignment cannot express on its own.
    pub note: Option<String>,
    pub differ: bool,
    /// A side could not be read or was refused, so the comparison is not authoritative.
    pub failed: bool,
}

impl FileComparison {
    /// Both sides as text, when both sides are text.
    pub fn texts(&self) -> Option<(&TextFile, &TextFile)> {
        Some((self.left.as_text()?, self.right.as_text()?))
    }
}

/// Load and compare two paths.
pub fn compare_files(
    left: &std::path::Path,
    right: &std::path::Path,
    max_size: u64,
    opts: &DiffOptions,
) -> FileComparison {
    let left = crate::text::load(left, max_size);
    let right = crate::text::load(right, max_size);
    compare_loaded(left, right, opts)
}

/// Compare two already-loaded sides.
pub fn compare_loaded(left: FileContent, right: FileContent, opts: &DiffOptions) -> FileComparison {
    match (left.as_text(), right.as_text()) {
        (Some(lt), Some(rt)) => {
            let alignment = align_text(lt, rt, opts);

            // Terminators are stripped before diffing, so a CRLF file and its LF twin
            // align as fully equal while the bytes plainly differ. Say so rather than
            // reporting "no differences".
            let eol_mismatch = lt.eol != rt.eol && lt.line_count() > 0 && rt.line_count() > 0;
            let newline_mismatch = lt.final_newline != rt.final_newline;

            let note = if eol_mismatch {
                Some(format!(
                    "line endings differ: {} vs {}",
                    lt.eol.label(),
                    rt.eol.label()
                ))
            } else if newline_mismatch && alignment.is_identical() {
                Some("one side has no newline at end of file".to_string())
            } else {
                None
            };

            let differ = !alignment.is_identical() || eol_mismatch || newline_mismatch;
            FileComparison {
                left,
                right,
                alignment: Some(alignment),
                note,
                differ,
                failed: false,
            }
        }
        _ => {
            let note = describe_undiffable(&left, &right);
            // Both readable but not line-diffable: still answer the "do they differ?"
            // question by comparing bytes.
            let differ = match (&left, &right) {
                (FileContent::Unreadable { .. }, _) | (_, FileContent::Unreadable { .. }) => true,
                _ => files_equal(left.path(), right.path()).map_or(true, |eq| !eq),
            };
            // A side we could not read or chose to refuse means the answer is not
            // authoritative, even when the byte comparison still produced one.
            let failed = matches!(
                (&left, &right),
                (
                    FileContent::Unreadable { .. } | FileContent::TooLarge { .. },
                    _
                ) | (
                    _,
                    FileContent::Unreadable { .. } | FileContent::TooLarge { .. }
                )
            );
            FileComparison {
                left,
                right,
                alignment: None,
                note: Some(note),
                differ,
                failed,
            }
        }
    }
}

fn describe_undiffable(left: &FileContent, right: &FileContent) -> String {
    let describe = |c: &FileContent| match c {
        FileContent::Text(_) => None,
        FileContent::Binary { reason, .. } => Some(match reason {
            crate::text::BinaryReason::Nul => "binary".to_string(),
            crate::text::BinaryReason::Bom(bom) => format!("{} (not decoded)", bom.label()),
        }),
        FileContent::TooLarge { size, limit, .. } => Some(format!(
            "{} exceeds the {} limit (--max-file-size)",
            human_size(*size),
            human_size(*limit)
        )),
        FileContent::Unreadable { message, .. } => Some(message.clone()),
    };

    match (describe(left), describe(right)) {
        (Some(l), Some(r)) if l == r => format!("both sides {l}"),
        (Some(l), Some(r)) => format!("left {l}; right {r}"),
        (Some(l), None) => format!("left {l}"),
        (None, Some(r)) => format!("right {r}"),
        (None, None) => "not comparable".to_string(),
    }
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// True when the two sides use different line terminators.
pub fn eol_differs(left: EolStyle, right: EolStyle) -> bool {
    left != right
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, RowKind, SideIdx};
    use crate::text::decode;
    use std::path::Path;

    fn file(body: &str) -> TextFile {
        decode(Path::new("test"), body.as_bytes().to_vec(), None)
    }

    /// Compact rendering of an alignment: left line, right line, kind.
    fn dump(a: &Alignment) -> String {
        let mut out = String::new();
        for row in a.rows() {
            let cell = |c: Cell| match c.line() {
                Some(n) => format!("{:>2}", n.display()),
                None => "  ".to_string(),
            };
            let kind = match row.kind {
                RowKind::Equal => '=',
                RowKind::Changed => '~',
                RowKind::Conflict => '!',
                RowKind::Unmatched => {
                    if row.cells[0].is_filler() {
                        '+'
                    } else {
                        '-'
                    }
                }
            };
            out.push_str(&format!(
                "{}|{} {kind}\n",
                cell(row.cells[0]),
                cell(row.cells[1])
            ));
        }
        out
    }

    fn align(left: &str, right: &str) -> Alignment {
        align_text(&file(left), &file(right), &DiffOptions::default())
    }

    #[test]
    fn identical_files_short_circuit_to_all_equal() {
        let a = align("one\ntwo\nthree\n", "one\ntwo\nthree\n");
        assert_eq!(dump(&a), " 1| 1 =\n 2| 2 =\n 3| 3 =\n");
        assert!(a.is_identical());
    }

    #[test]
    fn pure_insert_at_the_top() {
        let a = align("b\nc\n", "a\nb\nc\n");
        assert_eq!(dump(&a), "  | 1 +\n 1| 2 =\n 2| 3 =\n");
    }

    #[test]
    fn pure_delete_at_the_bottom() {
        let a = align("a\nb\nc\n", "a\nb\n");
        assert_eq!(dump(&a), " 1| 1 =\n 2| 2 =\n 3|   -\n");
    }

    #[test]
    fn replace_with_unequal_counts_pads_the_shorter_side() {
        let a = align("a\nX\nz\n", "a\nP\nQ\nR\nz\n");
        assert_eq!(dump(&a), " 1| 1 =\n 2| 2 ~\n  | 3 ~\n  | 4 ~\n 3| 5 =\n");
    }

    #[test]
    fn wholly_different_files() {
        let a = align("a\nb\n", "x\ny\nz\n");
        assert!(!a.is_identical());
        assert_eq!(a.len(), 3);
        assert_eq!(a.line_count(SideIdx::LEFT), 2);
        assert_eq!(a.line_count(SideIdx::RIGHT), 3);
    }

    #[test]
    fn empty_against_nonempty() {
        let a = align("", "a\nb\n");
        assert_eq!(dump(&a), "  | 1 +\n  | 2 +\n");
        let a = align("a\n", "");
        assert_eq!(dump(&a), " 1|   -\n");
    }

    #[test]
    fn both_empty_is_identical() {
        let a = align("", "");
        assert!(a.is_empty());
        assert!(a.is_identical());
    }

    #[test]
    fn inline_emphasis_marks_the_changed_words() {
        let a = align("the quick brown fox\n", "the quick red fox\n");
        assert_eq!(a.len(), 1);

        let left: Vec<_> = a.emphasis(0, SideIdx::LEFT).collect();
        let right: Vec<_> = a.emphasis(0, SideIdx::RIGHT).collect();
        assert!(!left.is_empty(), "expected emphasis on the left");
        assert!(!right.is_empty(), "expected emphasis on the right");

        // The emphasised span must cover the word that actually changed, and its offsets
        // must index the stripped line text directly.
        let line = "the quick brown fox";
        let covered: String = left
            .iter()
            .map(|e| &line[e.start as usize..e.end as usize])
            .collect();
        assert!(covered.contains("brown"), "covered {covered:?}");
    }

    #[test]
    fn no_emphasis_on_equal_rows() {
        let a = align("same\nchanged\n", "same\nCHANGED\n");
        assert_eq!(a.emphasis(0, SideIdx::LEFT).count(), 0);
        assert_eq!(a.emphasis(0, SideIdx::RIGHT).count(), 0);
    }

    #[test]
    fn inline_can_be_disabled() {
        let opts = DiffOptions {
            inline: false,
            ..DiffOptions::default()
        };
        let a = align_text(
            &file("the quick brown fox\n"),
            &file("the quick red fox\n"),
            &opts,
        );
        assert_eq!(a.emphasis(0, SideIdx::LEFT).count(), 0);
    }

    #[test]
    fn crlf_only_difference_aligns_as_equal_but_is_reported() {
        let left = FileContent::Text(Box::new(file("a\nb\n")));
        let right = FileContent::Text(Box::new(file("a\r\nb\r\n")));
        let cmp = compare_loaded(left, right, &DiffOptions::default());

        // The alignment sees no differences, because terminators are stripped...
        assert!(cmp.alignment.as_ref().unwrap().is_identical());
        // ...so the comparison itself has to carry the news.
        assert!(cmp.differ);
        assert_eq!(cmp.note.as_deref(), Some("line endings differ: LF vs CRLF"));
    }

    #[test]
    fn missing_trailing_newline_is_reported() {
        let left = FileContent::Text(Box::new(file("a\nb\n")));
        let right = FileContent::Text(Box::new(file("a\nb")));
        let cmp = compare_loaded(left, right, &DiffOptions::default());
        assert!(cmp.differ);
        assert_eq!(
            cmp.note.as_deref(),
            Some("one side has no newline at end of file")
        );
    }

    #[test]
    fn truly_identical_files_report_no_difference() {
        let left = FileContent::Text(Box::new(file("a\nb\n")));
        let right = FileContent::Text(Box::new(file("a\nb\n")));
        let cmp = compare_loaded(left, right, &DiffOptions::default());
        assert!(!cmp.differ);
        assert_eq!(cmp.note, None);
    }

    #[test]
    fn human_size_reads_sensibly() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(64 * 1024 * 1024), "64.0 MiB");
    }
}
