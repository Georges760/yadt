//! Copying hunks between the two sides, and undoing it.
//!
//! yadt reads files; this is the one place it changes them, and only in memory. Nothing
//! reaches the disk until an explicit save, and a save refuses if the file has moved
//! underneath it.
//!
//! An edit is stored as its own inverse — the line range now occupied and the lines that
//! were there before — rather than as a copy of the whole file. Undoing is then the same
//! splice run backwards, and the log costs the size of what changed rather than the size
//! of the file.

use std::ops::Range;

use crate::model::{Alignment, Hunk, SideIdx};
use crate::text::TextFile;

/// How many edits can be undone. Generous, since each one costs only the lines it
/// replaced.
pub const UNDO_LIMIT: usize = 256;

/// One applied edit, kept so it can be reversed.
#[derive(Clone, Debug)]
pub struct Edit {
    pub side: SideIdx,
    /// The lines the replacement now occupies.
    pub range: Range<usize>,
    /// What used to be there.
    pub previous: Vec<String>,
    /// The aligned row the cursor was on, so undo can put it back.
    pub cursor_row: u32,
}

/// The line range on `side` that `hunk` covers.
///
/// Empty when the side has no lines in the hunk — a pure insertion on the other side —
/// and positioned at exactly the point the new lines belong.
pub fn hunk_range(alignment: &Alignment, hunk: Hunk, side: SideIdx) -> Range<usize> {
    let start = alignment.line_at_or_after(hunk.start as usize, side) as usize;
    let end = alignment.line_at_or_after(hunk.end as usize, side) as usize;
    start..end.max(start)
}

/// The result of copying a hunk: the edited target file and the edit that made it.
pub struct Applied {
    pub file: TextFile,
    pub edit: Edit,
}

/// Copy one hunk from `from` to `to`.
///
/// Returns `None` when there is nothing to do — the two sides already agree over this
/// hunk, so an edit would only add an undo entry that changes nothing.
pub fn copy_hunk(
    alignment: &Alignment,
    hunk: Hunk,
    from: SideIdx,
    source: &TextFile,
    to: SideIdx,
    target: &TextFile,
    cursor_row: u32,
) -> Option<Applied> {
    let source_range = hunk_range(alignment, hunk, from);
    let target_range = hunk_range(alignment, hunk, to);

    let replacement: Vec<&str> = source_range
        .clone()
        .map(|i| source.line(crate::model::LineNo(i as u32)))
        .collect();
    let previous: Vec<String> = target_range
        .clone()
        .map(|i| target.line(crate::model::LineNo(i as u32)).to_string())
        .collect();

    if replacement.len() == previous.len()
        && replacement
            .iter()
            .zip(&previous)
            .all(|(new, old)| *new == old.as_str())
    {
        return None;
    }

    let file = target.splice_lines(target_range.clone(), &replacement);
    let edit = Edit {
        side: to,
        range: target_range.start..target_range.start + replacement.len(),
        previous,
        cursor_row,
    };
    Some(Applied { file, edit })
}

/// Reverse an edit, returning the restored file.
pub fn undo(edit: &Edit, current: &TextFile) -> TextFile {
    let previous: Vec<&str> = edit.previous.iter().map(String::as_str).collect();
    current.splice_lines(edit.range.clone(), &previous)
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

    fn body(f: &TextFile) -> String {
        String::from_utf8(f.to_bytes()).unwrap()
    }

    /// Copy every hunk in one direction and return the resulting target text.
    fn copy_all(left: &str, right: &str, from: SideIdx) -> String {
        let (mut l, mut r) = (file(left), file(right));
        loop {
            let alignment = align_text(&l, &r, &DiffOptions::default());
            let Some(&hunk) = alignment.hunks().first() else {
                break;
            };
            let (to, source, target) = if from == SideIdx::LEFT {
                (SideIdx::RIGHT, &l, &r)
            } else {
                (SideIdx::LEFT, &r, &l)
            };
            let Some(applied) = copy_hunk(&alignment, hunk, from, source, to, target, 0) else {
                break;
            };
            if to == SideIdx::RIGHT {
                r = applied.file;
            } else {
                l = applied.file;
            }
        }
        if from == SideIdx::LEFT {
            body(&r)
        } else {
            body(&l)
        }
    }

    #[test]
    fn copying_every_hunk_makes_the_sides_match() {
        let left = "one\nTWO\nthree\n";
        let right = "one\ntwo\nthree\n";
        assert_eq!(copy_all(left, right, SideIdx::LEFT), left);
        assert_eq!(copy_all(left, right, SideIdx::RIGHT), right);
    }

    #[test]
    fn a_pure_insertion_is_copied_to_the_right_place() {
        // The left has a line the right does not; copying left to right inserts it.
        let left = "a\nNEW\nb\n";
        let right = "a\nb\n";
        assert_eq!(copy_all(left, right, SideIdx::LEFT), left);
    }

    #[test]
    fn a_pure_deletion_is_copied_as_a_deletion() {
        // Copying the right onto the left must remove the line the left has extra.
        let left = "a\nGONE\nb\n";
        let right = "a\nb\n";
        assert_eq!(copy_all(left, right, SideIdx::RIGHT), right);
    }

    #[test]
    fn an_insertion_at_the_very_start_lands_before_everything() {
        let left = "FIRST\na\n";
        let right = "a\n";
        assert_eq!(copy_all(left, right, SideIdx::LEFT), left);
    }

    #[test]
    fn an_insertion_at_the_very_end_lands_after_everything() {
        let left = "a\nLAST\n";
        let right = "a\n";
        assert_eq!(copy_all(left, right, SideIdx::LEFT), left);
    }

    #[test]
    fn an_unequal_replacement_takes_the_whole_block() {
        let left = "a\nX\nY\nZ\nb\n";
        let right = "a\nq\nb\n";
        assert_eq!(copy_all(left, right, SideIdx::LEFT), left);
        assert_eq!(copy_all(left, right, SideIdx::RIGHT), right);
    }

    #[test]
    fn wholly_different_files_converge() {
        assert_eq!(copy_all("a\nb\nc\n", "x\ny\n", SideIdx::LEFT), "a\nb\nc\n");
        assert_eq!(copy_all("a\nb\nc\n", "x\ny\n", SideIdx::RIGHT), "x\ny\n");
    }

    #[test]
    fn copying_onto_an_identical_hunk_is_refused() {
        let (l, r) = (file("same\n"), file("same\n"));
        let alignment = align_text(&l, &r, &DiffOptions::default());
        // No hunks at all, so nothing to try; construct one over the equal row.
        let hunk = Hunk { start: 0, end: 1 };
        assert!(
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).is_none(),
            "an edit that changes nothing should not be recorded"
        );
    }

    #[test]
    fn undo_restores_the_previous_text_exactly() {
        let (l, r) = (file("a\nX\nb\n"), file("a\ny\nb\n"));
        let before = body(&r);
        let alignment = align_text(&l, &r, &DiffOptions::default());
        let hunk = alignment.hunks()[0];

        let applied =
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).unwrap();
        assert_eq!(body(&applied.file), "a\nX\nb\n");

        let restored = undo(&applied.edit, &applied.file);
        assert_eq!(body(&restored), before);
    }

    #[test]
    fn undo_restores_an_insertion_by_removing_it() {
        let (l, r) = (file("a\nNEW\nb\n"), file("a\nb\n"));
        let alignment = align_text(&l, &r, &DiffOptions::default());
        let hunk = alignment.hunks()[0];
        let applied =
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).unwrap();
        assert_eq!(body(&applied.file), "a\nNEW\nb\n");
        assert_eq!(body(&undo(&applied.edit, &applied.file)), "a\nb\n");
    }

    #[test]
    fn undo_restores_a_deletion_by_putting_the_lines_back() {
        let (l, r) = (file("a\nb\n"), file("a\nGONE\nb\n"));
        let alignment = align_text(&l, &r, &DiffOptions::default());
        let hunk = alignment.hunks()[0];
        let applied =
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).unwrap();
        assert_eq!(body(&applied.file), "a\nb\n");
        assert_eq!(body(&undo(&applied.edit, &applied.file)), "a\nGONE\nb\n");
    }

    #[test]
    fn crlf_is_preserved_when_lines_arrive_from_an_lf_file() {
        // The target decides how its own lines end, whatever the source used.
        let l = file("a\nNEW\nb\n");
        let r = file("a\r\nb\r\n");
        let alignment = align_text(&l, &r, &DiffOptions::default());
        let hunk = alignment.hunks()[0];
        let applied =
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).unwrap();
        assert_eq!(body(&applied.file), "a\r\nNEW\r\nb\r\n");
    }

    #[test]
    fn a_missing_final_newline_survives_an_edit() {
        let l = file("a\nX\n");
        let r = file("a\ny");
        assert!(!r.final_newline);
        let alignment = align_text(&l, &r, &DiffOptions::default());
        let hunk = alignment.hunks()[0];
        let applied =
            copy_hunk(&alignment, hunk, SideIdx::LEFT, &l, SideIdx::RIGHT, &r, 0).unwrap();
        assert_eq!(
            body(&applied.file),
            "a\nX",
            "the file still ends without one"
        );
    }
}
