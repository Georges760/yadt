//! Folding a `similar` op-stream into an [`Alignment`].
//!
//! This is the **only** module that names `similar`. Nothing downstream ever sees a
//! `DiffOp`, which is what keeps the diff algorithm swappable: replacing it means
//! rewriting [`align_text`] and nothing else.

use std::borrow::Cow;
use std::time::Duration;

use similar::{Algorithm, ChangeTag, DiffOp, TextDiff};

use crate::model::{Alignment, AlignmentBuilder, Cell, LineNo, MAX_SIDES, RowKind, SideIdx};
use crate::text::{BinaryReason, EolStyle, FileContent, TextFile, files_equal};

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
    pub whitespace: Whitespace,
    /// Bytes per row when two binaries are compared as hex.
    pub hex_bytes_per_row: u32,
}

/// How much whitespace difference counts as a difference.
///
/// `similar`'s own `WhitespaceMode` only applies to `diff_lines`, which re-tokenizes the
/// text and would undo the stripped-terminator invariant this module depends on. So the
/// normalisation happens here, on a throwaway copy used purely for comparison — the
/// original lines are what get rendered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Whitespace {
    #[default]
    Exact,
    /// Collapse runs of whitespace and ignore trailing whitespace, like `diff -b`.
    IgnoreChanges,
    /// Ignore whitespace entirely, like `diff -w`.
    IgnoreAll,
}

impl Whitespace {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::IgnoreChanges => "ignore-changes",
            Self::IgnoreAll => "ignore-all",
        }
    }
}

/// Two lines that word-diff below this ratio are too dissimilar for per-word emphasis to
/// be signal rather than noise.
const INLINE_MIN_RATIO: f32 = 0.5;

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            // git's default, and for the same reason: it resists the "diff anchored on the
            // closing brace" pathology that Myers hits on code.
            algorithm: Algorithm::Histogram,
            timeout: DEFAULT_TIMEOUT,
            inline: true,
            whitespace: Whitespace::Exact,
            hex_bytes_per_row: crate::hex::DEFAULT_BYTES_PER_ROW,
        }
    }
}

/// The form of a line used for *comparison*. Never rendered.
fn normalize(line: &str, mode: Whitespace) -> Cow<'_, str> {
    match mode {
        Whitespace::Exact => Cow::Borrowed(line),
        Whitespace::IgnoreAll => {
            if line.bytes().any(|b| b.is_ascii_whitespace()) {
                Cow::Owned(line.split_whitespace().collect())
            } else {
                Cow::Borrowed(line)
            }
        }
        Whitespace::IgnoreChanges => {
            // Only a line with no whitespace at all is certainly already in normal form.
            // A single tab looks normal by count but is not a single *space*, which is
            // what normal form means here.
            if !line.bytes().any(|b| b.is_ascii_whitespace()) {
                return Cow::Borrowed(line);
            }
            // Every run collapses to one space and trailing whitespace goes, but a run is
            // never removed outright: GNU `diff -b` calls "  a" and "a" different, because
            // that is a change in presence rather than in amount. Verified against
            // `diff -b` for exactly these cases. Use `IgnoreAll` to ignore indentation.
            let trimmed = line.trim_end();
            let mut out = String::with_capacity(trimmed.len());
            let mut in_run = false;
            for c in trimmed.chars() {
                if c.is_whitespace() {
                    if !in_run {
                        out.push(' ');
                        in_run = true;
                    }
                } else {
                    out.push(c);
                    in_run = false;
                }
            }
            Cow::Owned(out)
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

    // The lines as rendered. Emphasis offsets index these, always.
    let lo = left.slices();
    let ln = right.slices();

    // The lines as compared. Identical to the above unless whitespace is being ignored,
    // in which case comparison runs on a normalised copy that is never displayed.
    let cmp_l: Vec<Cow<'_, str>> = lo.iter().map(|l| normalize(l, opts.whitespace)).collect();
    let cmp_r: Vec<Cow<'_, str>> = ln.iter().map(|l| normalize(l, opts.whitespace)).collect();
    let cmp_l: Vec<&str> = cmp_l.iter().map(Cow::as_ref).collect();
    let cmp_r: Vec<&str> = cmp_r.iter().map(Cow::as_ref).collect();

    let diff = TextDiff::configure()
        .algorithm(opts.algorithm)
        .timeout(opts.timeout)
        // Lines are already stripped of their terminators, so inline byte offsets are
        // offsets into exactly the `&str` the renderer draws.
        .newline_terminated(false)
        .diff_slices(&cmp_l, &cmp_r);

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
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                // Which lines belong opposite which is decided by similarity, not by
                // position: see `layout_replace`.
                let rows = layout_replace(
                    old_index,
                    &cmp_l[old_index..old_index + old_len],
                    new_index,
                    &cmp_r[new_index..new_index + new_len],
                );
                let block = b.push_paired(&rows);
                if opts.inline {
                    // Emphasise only the rows that actually paired, using the *original*
                    // lines -- so the highlight lines up with what is drawn, whatever the
                    // whitespace mode did to the comparison.
                    for (offset, (kind, cells)) in rows.iter().enumerate() {
                        let (Some(left), Some(right)) = (
                            cells[SideIdx::LEFT.idx()].line(),
                            cells[SideIdx::RIGHT.idx()].line(),
                        ) else {
                            continue;
                        };
                        if *kind != RowKind::Changed {
                            continue;
                        }
                        emphasize(
                            &mut b,
                            block.start + offset as u32,
                            lo[left.0 as usize],
                            ln[right.0 as usize],
                        );
                    }
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

/// Mark the words that differ between one pair of lines.
///
/// Word-diffing the pair directly, rather than using `iter_inline_changes` over the whole
/// replace block, has two advantages: the offsets index the original lines by
/// construction, so they survive whitespace normalisation; and the emphasis matches the
/// rows as actually paired for display.
fn emphasize(b: &mut AlignmentBuilder, row: u32, left: &str, right: &str) {
    if left == right {
        return;
    }
    let words = TextDiff::configure().diff_unicode_words(left, right);
    // Two wholly unrelated lines highlight almost every word, which is noise. Leave the
    // row plainly marked as changed instead.
    if words.ratio() < INLINE_MIN_RATIO {
        return;
    }

    let (mut left_at, mut right_at) = (0u32, 0u32);
    let (mut left_spans, mut right_spans) = (Vec::new(), Vec::new());
    for change in words.iter_all_changes() {
        let len = change.value().len() as u32;
        match change.tag() {
            ChangeTag::Equal => {
                left_at += len;
                right_at += len;
            }
            ChangeTag::Delete => {
                left_spans.push((left_at, left_at + len));
                left_at += len;
            }
            ChangeTag::Insert => {
                right_spans.push((right_at, right_at + len));
                right_at += len;
            }
        }
    }
    b.set_emphasis(row, SideIdx::LEFT, left_spans);
    b.set_emphasis(row, SideIdx::RIGHT, right_spans);
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
    /// `Some(bytes_per_row)` when the alignment is over bytes rather than lines.
    pub hex: Option<u32>,
    /// What each side looked like on disk when it was read, so a save can refuse to
    /// overwrite something that has changed underneath.
    pub stamps: [Option<crate::text::Stamp>; 2],
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
    let stamps = [
        crate::text::Stamp::of(left).ok(),
        crate::text::Stamp::of(right).ok(),
    ];
    let left = crate::text::load(left, max_size);
    let right = crate::text::load(right, max_size);
    let mut cmp = compare_loaded(left, right, opts);
    cmp.stamps = stamps;
    cmp
}

/// Compare two already-loaded sides.
pub fn compare_loaded(left: FileContent, right: FileContent, opts: &DiffOptions) -> FileComparison {
    // Two binaries get a hex diff rather than a bare "they differ": the whole point of
    // opening one is to see *where*.
    if let (Some(lb), Some(rb)) = (left.as_binary(), right.as_binary()) {
        let alignment = crate::hex::align_bytes(lb, rb, opts.hex_bytes_per_row);
        let differ = lb != rb;
        let note = describe_binary(&left, &right);
        return FileComparison {
            left,
            right,
            alignment: Some(alignment),
            note,
            differ,
            failed: false,
            hex: Some(opts.hex_bytes_per_row.max(1)),
            stamps: [None; 2],
        };
    }

    match (left.as_text(), right.as_text()) {
        (Some(lt), Some(rt)) => {
            let alignment = align_text(lt, rt, opts);

            // Terminators are stripped before diffing, so a CRLF file and its LF twin
            // align as fully equal while the bytes plainly differ. Say so rather than
            // reporting "no differences".
            let eol_mismatch = lt.eol != rt.eol && lt.line_count() > 0 && rt.line_count() > 0;
            let newline_mismatch = lt.final_newline != rt.final_newline;

            // Ignoring whitespace can make files that plainly differ align as equal. That is
            // the point of the flag -- `diff -w` exits 0 too -- but say so rather than
            // leaving it looking like the files match byte for byte.
            let whitespace_only = opts.whitespace != Whitespace::Exact
                && alignment.is_identical()
                && lt.text != rt.text;

            let note = if whitespace_only {
                Some(format!(
                    "identical apart from whitespace ({})",
                    opts.whitespace.label()
                ))
            } else if eol_mismatch {
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
                hex: None,
                stamps: [None; 2],
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
                hex: None,
                stamps: [None; 2],
            }
        }
    }
}

/// Why these two are being shown as hex, when the reason is not simply "both binary".
fn describe_binary(left: &FileContent, right: &FileContent) -> Option<String> {
    let bom = |c: &FileContent| match c {
        FileContent::Binary {
            reason: BinaryReason::Bom(bom),
            ..
        } => Some(bom.label()),
        _ => None,
    };
    match (bom(left), bom(right)) {
        (None, None) => None,
        (l, r) => Some(format!(
            "{} -- shown as hex, not decoded",
            match (l, r) {
                (Some(l), Some(r)) if l == r => l.to_string(),
                (Some(l), Some(r)) => format!("left {l}, right {r}"),
                (Some(l), None) => format!("left {l}"),
                (None, Some(r)) => format!("right {r}"),
                (None, None) => unreachable!(),
            }
        )),
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
        // X pairs with P, and the two extra lines are additions rather than
        // modifications of nothing -- a row with a filler side is never `Changed`.
        let a = align("a\nX\nz\n", "a\nP\nQ\nR\nz\n");
        assert_eq!(dump(&a), " 1| 1 =\n 2| 2 ~\n  | 3 +\n  | 4 +\n 3| 5 =\n");
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

#[cfg(test)]
mod whitespace_tests {
    use super::*;
    use crate::model::SideIdx;
    use crate::text::decode;
    use std::path::Path;

    fn file(body: &str) -> TextFile {
        decode(Path::new("test"), body.as_bytes().to_vec(), None)
    }

    fn opts(whitespace: Whitespace) -> DiffOptions {
        DiffOptions {
            whitespace,
            ..DiffOptions::default()
        }
    }

    fn identical(left: &str, right: &str, whitespace: Whitespace) -> bool {
        align_text(&file(left), &file(right), &opts(whitespace)).is_identical()
    }

    #[test]
    fn exact_mode_sees_every_whitespace_difference() {
        assert!(!identical("a  b\n", "a b\n", Whitespace::Exact));
        assert!(!identical("a b \n", "a b\n", Whitespace::Exact));
        assert!(!identical("  a\n", "a\n", Whitespace::Exact));
    }

    #[test]
    fn ignore_changes_collapses_runs_and_drops_trailing_space() {
        assert!(identical("a  b\n", "a b\n", Whitespace::IgnoreChanges));
        assert!(identical("a b   \n", "a b\n", Whitespace::IgnoreChanges));
        assert!(identical("a\tb\n", "a b\n", Whitespace::IgnoreChanges));
        // Amount ignored, presence not -- matching GNU `diff -b`, verified against it.
        assert!(identical("  a\n", "    a\n", Whitespace::IgnoreChanges));
        assert!(!identical("  a\n", "a\n", Whitespace::IgnoreChanges));
        // A real difference is still a difference.
        assert!(!identical("a b\n", "a c\n", Whitespace::IgnoreChanges));
        // Whitespace between words cannot vanish entirely: "ab" is not "a b".
        assert!(!identical("ab\n", "a b\n", Whitespace::IgnoreChanges));
    }

    #[test]
    fn ignore_all_removes_whitespace_entirely() {
        assert!(identical("ab\n", "a b\n", Whitespace::IgnoreAll));
        assert!(identical("  a\tb  \n", "ab\n", Whitespace::IgnoreAll));
        assert!(!identical("ab\n", "ac\n", Whitespace::IgnoreAll));
    }

    #[test]
    fn reindentation_needs_ignore_all_not_ignore_changes() {
        // Indenting a previously-unindented line adds whitespace where there was none,
        // which `diff -b` treats as a difference. Only `-w` hides it, and yadt agrees.
        let left = "fn main() {\nprintln!();\n}\n";
        let right = "fn main() {\n    println!();\n}\n";
        assert!(!identical(left, right, Whitespace::Exact));
        assert!(!identical(left, right, Whitespace::IgnoreChanges));
        assert!(identical(left, right, Whitespace::IgnoreAll));
    }

    #[test]
    fn changing_the_depth_of_existing_indentation_is_ignored_by_changes() {
        let left = "fn main() {\n  println!();\n}\n";
        let right = "fn main() {\n        println!();\n}\n";
        assert!(!identical(left, right, Whitespace::Exact));
        assert!(identical(left, right, Whitespace::IgnoreChanges));
    }

    #[test]
    fn normalize_borrows_when_a_line_is_already_normal() {
        // A line with no whitespace is certainly already normal, so it never allocates.
        assert!(matches!(
            normalize("abc", Whitespace::IgnoreChanges),
            Cow::Borrowed(_)
        ));
        // A lone tab looks normal by count but is not a single space.
        assert_eq!(normalize("a\tb", Whitespace::IgnoreChanges), "a b");
        // A leading run collapses to one space rather than vanishing.
        assert_eq!(normalize("    a", Whitespace::IgnoreChanges), " a");
        assert_eq!(normalize("a   ", Whitespace::IgnoreChanges), "a");
        assert!(matches!(
            normalize("abc", Whitespace::IgnoreAll),
            Cow::Borrowed(_)
        ));
        assert!(matches!(
            normalize("a  b", Whitespace::IgnoreChanges),
            Cow::Owned(_)
        ));
    }

    #[test]
    fn a_whitespace_only_difference_is_reported_rather_than_hidden() {
        let cmp = compare_loaded(
            FileContent::Text(Box::new(file("a  b\n"))),
            FileContent::Text(Box::new(file("a b\n"))),
            &opts(Whitespace::IgnoreChanges),
        );
        // Like `diff -w`, the verdict is "same"...
        assert!(!cmp.differ);
        // ...but the reason is on screen, not silently swallowed.
        assert_eq!(
            cmp.note.as_deref(),
            Some("identical apart from whitespace (ignore-changes)")
        );
    }

    #[test]
    fn truly_identical_files_get_no_whitespace_note() {
        let cmp = compare_loaded(
            FileContent::Text(Box::new(file("a b\n"))),
            FileContent::Text(Box::new(file("a b\n"))),
            &opts(Whitespace::IgnoreChanges),
        );
        assert!(!cmp.differ);
        assert_eq!(cmp.note, None);
    }

    #[test]
    fn emphasis_offsets_index_the_original_line_not_the_normalised_one() {
        // The left line has extra interior spacing, so normalisation shortens it. If
        // emphasis were computed on the normalised text the offsets would land short.
        let a = align_text(
            &file("let  x   =  alpha;\n"),
            &file("let x = beta;\n"),
            &opts(Whitespace::IgnoreChanges),
        );
        let left_line = "let  x   =  alpha;";
        let covered: String = a
            .emphasis(0, SideIdx::LEFT)
            .map(|e| &left_line[e.start as usize..e.end as usize])
            .collect();
        assert!(
            covered.contains("alpha"),
            "emphasis covered {covered:?}, which is not the changed word"
        );
    }
}

// ---------------------------------------------------------------------------------------
// Replace-block pairing
// ---------------------------------------------------------------------------------------

/// A pair below this similarity is not a modification of the same line, just two different
/// lines that happen to be adjacent.
const MIN_PAIR_RATIO: f32 = 0.4;

/// Above this many candidate pairs, fall back to index order. The search is O(n*m) diffs,
/// and a replace block that large is a rewrite where the pairing hardly matters.
const PAIR_SEARCH_LIMIT: usize = 400;

/// How alike two lines are, as a word-level ratio.
fn similarity(old: &str, new: &str) -> f32 {
    if old == new {
        return 1.0;
    }
    // A blank line shares no words with anything, so this is mostly a shortcut -- but it
    // also stops an added blank line from anchoring against real code.
    if old.trim().is_empty() || new.trim().is_empty() {
        return 0.0;
    }
    TextDiff::configure().diff_unicode_words(old, new).ratio()
}

/// Which lines of a replace block are modifications of each other.
///
/// Returns the index pairs, in order. Everything not paired is an insertion or a deletion.
///
/// Index order is only right when both sides changed in lockstep. As soon as one side
/// gains a line, pairing by position makes the addition look like a modification of
/// whatever happens to sit opposite it -- and worse, makes the line it really corresponds
/// to look unmatched.
fn anchor_pairs(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let (n, m) = (old.len(), new.len());
    if n == 0 || m == 0 || n * m > PAIR_SEARCH_LIMIT {
        return Vec::new();
    }

    // Best total similarity obtainable from (i, j) onwards, and how it was reached.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Step {
        Pair,
        SkipOld,
        SkipNew,
    }
    let mut score = vec![vec![0f32; m + 1]; n + 1];
    let mut step = vec![vec![Step::SkipOld; m + 1]; n + 1];

    for i in (0..=n).rev() {
        for j in (0..=m).rev() {
            if i == n && j == m {
                continue;
            }
            let mut best = f32::NEG_INFINITY;
            let mut pick = Step::SkipOld;
            if i < n && score[i + 1][j] > best {
                best = score[i + 1][j];
                pick = Step::SkipOld;
            }
            if j < m && score[i][j + 1] > best {
                best = score[i][j + 1];
                pick = Step::SkipNew;
            }
            if i < n && j < m {
                let ratio = similarity(old[i], new[j]);
                // Strictly better, so an equally-good pairing loses to leaving the lines
                // unmatched -- which reads more honestly.
                if ratio >= MIN_PAIR_RATIO && ratio + score[i + 1][j + 1] > best {
                    best = ratio + score[i + 1][j + 1];
                    pick = Step::Pair;
                }
            }
            score[i][j] = best;
            step[i][j] = pick;
        }
    }

    let mut pairs = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        match step[i][j] {
            Step::Pair => {
                pairs.push((i, j));
                i += 1;
                j += 1;
            }
            Step::SkipOld => i += 1,
            Step::SkipNew => j += 1,
        }
    }
    pairs
}

/// Lay a replace block out as rows.
///
/// Anchored pairs line up; the leftovers between two anchors are zipped index-wise, which
/// keeps a wholly rewritten block as compact as it was before. A row with a filler on one
/// side is `Unmatched`, never `Changed`: it is an addition or a deletion, and the marker
/// column has to say so.
fn layout_replace(
    old_index: usize,
    old: &[&str],
    new_index: usize,
    new: &[&str],
) -> Vec<(RowKind, [Cell; MAX_SIDES])> {
    let cell = |base: usize, offset: usize| Cell::Line(LineNo((base + offset) as u32));
    let mut rows = Vec::new();

    let emit_gap = |rows: &mut Vec<(RowKind, [Cell; MAX_SIDES])>,
                    old_range: std::ops::Range<usize>,
                    new_range: std::ops::Range<usize>| {
        let height = old_range.len().max(new_range.len());
        for k in 0..height {
            let mut cells = [Cell::Filler; MAX_SIDES];
            let has_old = k < old_range.len();
            let has_new = k < new_range.len();
            if has_old {
                cells[SideIdx::LEFT.idx()] = cell(old_index, old_range.start + k);
            }
            if has_new {
                cells[SideIdx::RIGHT.idx()] = cell(new_index, new_range.start + k);
            }
            let kind = if has_old && has_new {
                RowKind::Changed
            } else {
                RowKind::Unmatched
            };
            rows.push((kind, cells));
        }
    };

    let (mut i, mut j) = (0, 0);
    for (pi, pj) in anchor_pairs(old, new) {
        emit_gap(&mut rows, i..pi, j..pj);
        let mut cells = [Cell::Filler; MAX_SIDES];
        cells[SideIdx::LEFT.idx()] = cell(old_index, pi);
        cells[SideIdx::RIGHT.idx()] = cell(new_index, pj);
        rows.push((RowKind::Changed, cells));
        i = pi + 1;
        j = pj + 1;
    }
    emit_gap(&mut rows, i..old.len(), j..new.len());
    rows
}

#[cfg(test)]
mod pairing_tests {
    use super::*;
    use crate::text::decode;
    use std::path::Path;

    fn file(body: &str) -> TextFile {
        decode(Path::new("t"), body.as_bytes().to_vec(), None)
    }

    /// Render as `left | right kind`, which is what the user actually sees.
    fn layout(left: &str, right: &str) -> String {
        let (l, r) = (file(left), file(right));
        let a = align_text(&l, &r, &DiffOptions::default());
        a.rows()
            .iter()
            .map(|row| {
                let show = |c: Cell, f: &TextFile| {
                    c.line()
                        .map_or_else(|| "~".to_string(), |n| f.line(n).to_string())
                };
                let kind = match row.kind {
                    RowKind::Equal => "=",
                    RowKind::Changed => "~",
                    RowKind::Conflict => "!",
                    RowKind::Unmatched => {
                        if row.cells[0].is_filler() {
                            "+"
                        } else {
                            "-"
                        }
                    }
                };
                format!(
                    "{}|{} {kind}\n",
                    show(row.cells[0], &l),
                    show(row.cells[1], &r)
                )
            })
            .collect()
    }

    #[test]
    fn a_line_inserted_before_a_changed_one_is_not_paired_with_it() {
        // The bug this fixes: "zzz" was shown opposite "let x = 1;" and the line that
        // really corresponds to it was left looking unmatched.
        assert_eq!(
            layout("a\nzzz\nlet x = 2;\nb\n", "a\nlet x = 1;\nb\n"),
            "a|a =\nzzz|~ -\nlet x = 2;|let x = 1; ~\nb|b =\n"
        );
    }

    #[test]
    fn an_added_blank_line_anchors_against_nothing() {
        assert_eq!(
            layout("a\n\nlet x = 2;\n", "a\nlet x = 1;\n"),
            "a|a =\n|~ -\nlet x = 2;|let x = 1; ~\n"
        );
    }

    #[test]
    fn several_added_lines_before_a_change_stay_additions() {
        assert_eq!(
            layout("a\nN1\nN2\nvalue = 9\nb\n", "a\nvalue = 1\nb\n"),
            "a|a =\nN1|~ -\nN2|~ -\nvalue = 9|value = 1 ~\nb|b =\n"
        );
    }

    #[test]
    fn a_line_removed_before_a_change_is_a_deletion() {
        assert_eq!(
            layout("a\nvalue = 1\nb\n", "a\nGONE\nvalue = 9\nb\n"),
            "a|a =\n~|GONE +\nvalue = 1|value = 9 ~\nb|b =\n"
        );
    }

    #[test]
    fn a_wholly_rewritten_block_stays_compact() {
        // Nothing pairs, so the old index-order zip still applies: three rows, not six.
        assert_eq!(
            layout("a\nqqq\nwww\neee\nb\n", "a\n111\n222\n333\nb\n"),
            "a|a =\nqqq|111 ~\nwww|222 ~\neee|333 ~\nb|b =\n"
        );
    }

    #[test]
    fn lockstep_changes_still_pair_by_position() {
        assert_eq!(
            layout("let a = 1;\nlet b = 2;\n", "let a = 9;\nlet b = 8;\n"),
            "let a = 1;|let a = 9; ~\nlet b = 2;|let b = 8; ~\n"
        );
    }

    #[test]
    fn a_filler_row_is_never_marked_as_a_change() {
        let (l, r) = (file("a\nX\nz\n"), file("a\nP\nQ\nR\nz\n"));
        let a = align_text(&l, &r, &DiffOptions::default());
        for (i, row) in a.rows().iter().enumerate() {
            let has_filler = row.cells[..2].iter().any(|c| c.is_filler());
            assert!(
                !(has_filler && row.kind == RowKind::Changed),
                "row {i} has a filler side but claims to be a modification"
            );
        }
    }

    #[test]
    fn emphasis_lands_on_the_paired_row_not_the_inserted_one() {
        let (l, r) = (file("a\nzzz\nlet x = 2;\n"), file("a\nlet x = 1;\n"));
        let a = align_text(&l, &r, &DiffOptions::default());
        // Row 1 is the insertion; row 2 is the real modification.
        assert_eq!(a.emphasis(1, SideIdx::LEFT).count(), 0);
        assert!(a.emphasis(2, SideIdx::LEFT).count() > 0);
    }

    #[test]
    fn a_very_large_replace_block_falls_back_without_hanging() {
        // Past the search limit the pairing is index order again, which is fine: a block
        // that big is a rewrite.
        let left: String = (0..40).map(|i| format!("old line {i}\n")).collect();
        let right: String = (0..40).map(|i| format!("new line {i}\n")).collect();
        let a = align_text(&file(&left), &file(&right), &DiffOptions::default());
        assert_eq!(a.len(), 40, "should stay one row per pair");
    }

    #[test]
    fn similarity_scores_what_it_should() {
        assert_eq!(similarity("same", "same"), 1.0);
        assert_eq!(similarity("", "let x = 1;"), 0.0);
        assert_eq!(similarity("   ", "let x = 1;"), 0.0);
        assert!(similarity("let x = 1;", "let x = 2;") > MIN_PAIR_RATIO);
        assert!(similarity("zzz", "let x = 1;") < MIN_PAIR_RATIO);
    }
}
