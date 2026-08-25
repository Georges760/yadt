//! The alignment data model: a diff rendered as rows aligned across N sides.
//!
//! This module has zero external dependencies on purpose. It knows nothing about
//! `similar`, nothing about ratatui, and nothing about the filesystem — which is what
//! makes the diff algorithm swappable and these tests instant.
//!
//! The invariant everything rests on:
//!
//! > Aligned row `i` refers to at most one source line per side, and for any fixed side
//! > the present line indices are exactly `0, 1, ... , line_count-1`, in order, with no
//! > gaps and no repeats.
//!
//! Both panes render `rows[top .. top + height]`, so synchronized scrolling is not a
//! feature — it is a consequence of there being a single `top`.

use std::ops::Range;

/// Number of sides an [`Alignment`] can hold. 2 today; 3 leaves room for a base/left/right
/// three-way comparison without changing any of the types below.
pub const MAX_SIDES: usize = 3;

/// Which side of a comparison a value belongs to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SideIdx(pub u8);

impl SideIdx {
    pub const LEFT: Self = Self(0);
    pub const RIGHT: Self = Self(1);
    /// Reserved for three-way comparison.
    pub const BASE: Self = Self(2);

    pub const fn idx(self) -> usize {
        self.0 as usize
    }
}

/// A zero-based index into one side's line table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct LineNo(pub u32);

impl LineNo {
    /// The one-based number to print in the gutter.
    pub const fn display(self) -> u32 {
        self.0 + 1
    }
}

/// What one side shows on one row.
///
/// `Filler` means this side has no line here: the gutter prints blank and the pane paints
/// the filler background. Making this an enum rather than a `usize` with a sentinel is why
/// the gutter never needs a lookup table.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Cell {
    #[default]
    Filler,
    Line(LineNo),
}

impl Cell {
    pub const fn line(self) -> Option<LineNo> {
        match self {
            Self::Line(n) => Some(n),
            Self::Filler => None,
        }
    }

    pub const fn is_filler(self) -> bool {
        matches!(self, Self::Filler)
    }
}

/// Semantic class of a row.
///
/// Per-side styling is derived from `kind` plus `cells[i].is_filler()` — never from a
/// "left" or "right" field, so a third column needs no new variants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    /// Every present side has identical content.
    Equal,
    /// Two or more sides present, contents differ. May carry inline emphasis.
    Changed,
    /// Present on some sides, filler on others (a pure insert or delete).
    Unmatched,
    /// Reserved for three-way: sides disagree in a way with no two-way analogue.
    Conflict,
}

impl RowKind {
    pub const fn is_equal(self) -> bool {
        matches!(self, Self::Equal)
    }
}

/// A byte range inside one side's source line that differs from its counterpart.
///
/// Offsets are into the *stripped* line text — the same `&str` the renderer draws — so no
/// terminator arithmetic is ever needed to use them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Emph {
    pub side: SideIdx,
    pub start: u32,
    pub end: u32,
}

/// One screen row, aligned across every side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub kind: RowKind,
    pub cells: [Cell; MAX_SIDES],
    /// Slice into [`Alignment::emph`]. Empty for everything but `Changed`.
    emph_start: u32,
    emph_len: u16,
}

impl Row {
    fn emph_range(&self) -> Range<usize> {
        let start = self.emph_start as usize;
        start..start + self.emph_len as usize
    }
}

/// A run of contiguous non-`Equal` rows, for `n`/`p` navigation.
///
/// Stored as two fields rather than a `Range<u32>` so that `Hunk` stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hunk {
    pub start: u32,
    pub end: u32,
}

impl Hunk {
    pub const fn rows(self) -> Range<usize> {
        self.start as usize..self.end as usize
    }

    pub const fn height(self) -> u32 {
        self.end - self.start
    }
}

/// Row counts by class. `inserted`/`deleted` are meaningful for two-way comparisons.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DiffStats {
    pub equal: u32,
    pub changed: u32,
    pub inserted: u32,
    pub deleted: u32,
}

/// A computed diff, as rows aligned across N sides.
#[derive(Clone, Debug)]
pub struct Alignment {
    sides: u8,
    rows: Vec<Row>,
    emph: Vec<Emph>,
    hunks: Vec<Hunk>,
    line_counts: [u32; MAX_SIDES],
}

impl Alignment {
    pub fn sides(&self) -> usize {
        self.sides as usize
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn row(&self, row: usize) -> Option<&Row> {
        self.rows.get(row)
    }

    pub fn line_count(&self, side: SideIdx) -> u32 {
        self.line_counts[side.idx()]
    }

    /// What `side` shows on `row`. Out-of-range rows read as `Filler`.
    pub fn cell(&self, row: usize, side: SideIdx) -> Cell {
        self.rows
            .get(row)
            .map_or(Cell::Filler, |r| r.cells[side.idx()])
    }

    /// Inline emphasis for one (row, side). Empty for non-`Changed` rows.
    pub fn emphasis(&self, row: usize, side: SideIdx) -> impl Iterator<Item = &Emph> {
        let range = self.rows.get(row).map_or(0..0, Row::emph_range);
        self.emph[range].iter().filter(move |e| e.side == side)
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    /// The first hunk starting at or after `row`.
    pub fn hunk_at_or_after(&self, row: usize) -> Option<Hunk> {
        let i = self.hunks.partition_point(|h| (h.start as usize) < row);
        self.hunks.get(i).copied()
    }

    /// The last hunk starting strictly before `row`.
    pub fn hunk_before(&self, row: usize) -> Option<Hunk> {
        let i = self.hunks.partition_point(|h| (h.start as usize) < row);
        i.checked_sub(1).and_then(|i| self.hunks.get(i)).copied()
    }

    /// Which row shows `line` on `side`.
    ///
    /// `O(log n)` with no inverse index, which works precisely because of the module
    /// invariant: present line indices increase monotonically down any given side.
    pub fn row_of_line(&self, side: SideIdx, line: LineNo) -> Option<usize> {
        let s = side.idx();
        let i = self.rows.partition_point(|r| match r.cells[s] {
            Cell::Line(n) => n < line,
            // Fillers belong with the line that precedes them.
            Cell::Filler => true,
        });
        (self.rows.get(i)?.cells[s] == Cell::Line(line)).then_some(i)
    }

    /// True when the two sides are the same all the way down.
    pub fn is_identical(&self) -> bool {
        self.hunks.is_empty()
    }

    pub fn stats(&self) -> DiffStats {
        let mut stats = DiffStats::default();
        for row in &self.rows {
            match row.kind {
                RowKind::Equal => stats.equal += 1,
                RowKind::Changed | RowKind::Conflict => stats.changed += 1,
                RowKind::Unmatched => {
                    if row.cells[SideIdx::LEFT.idx()].is_filler() {
                        stats.inserted += 1;
                    } else {
                        stats.deleted += 1;
                    }
                }
            }
        }
        stats
    }
}

/// Builds an [`Alignment`] one block at a time.
///
/// Callers push blocks in source order; the builder tracks how far into each side it has
/// got, so nothing upstream ever does line-index arithmetic.
pub struct AlignmentBuilder {
    sides: u8,
    rows: Vec<Row>,
    /// Collected out of order, then sorted into contiguous per-row slices by `finish`.
    pending_emph: Vec<(u32, Emph)>,
    /// Next unconsumed source line, per side.
    cursor: [u32; MAX_SIDES],
}

impl AlignmentBuilder {
    /// # Panics
    /// If `sides` is 0 or greater than [`MAX_SIDES`].
    pub fn new(sides: usize) -> Self {
        assert!(
            (1..=MAX_SIDES).contains(&sides),
            "sides must be 1..={MAX_SIDES}, got {sides}"
        );
        Self {
            sides: sides as u8,
            rows: Vec::new(),
            pending_emph: Vec::new(),
            cursor: [0; MAX_SIDES],
        }
    }

    fn side_count(&self) -> usize {
        self.sides as usize
    }

    /// Emit `len` rows in which every side advances together.
    pub fn push_equal(&mut self, len: u32) -> Range<u32> {
        self.push_block(RowKind::Equal, [len; MAX_SIDES])
    }

    /// Emit a block consuming `counts[s]` lines from side `s`.
    ///
    /// The block is `counts.iter().max()` rows tall; sides with fewer lines get
    /// [`Cell::Filler`] in the trailing rows. This one function is the whole answer to
    /// "pad the shorter side of a replace block", and it is N-ary by construction:
    /// `[3, 5, 0]` yields 5 rows with fillers on side 0 in rows 3..5, and `[4, 0, 0]`
    /// (a pure delete) yields 4 rows with fillers on side 1.
    pub fn push_block(&mut self, kind: RowKind, counts: [u32; MAX_SIDES]) -> Range<u32> {
        let sides = self.side_count();
        let height = counts[..sides].iter().copied().max().unwrap_or(0);
        let start = self.rows.len() as u32;

        for k in 0..height {
            let mut cells = [Cell::Filler; MAX_SIDES];
            for (s, cell) in cells[..sides].iter_mut().enumerate() {
                if k < counts[s] {
                    *cell = Cell::Line(LineNo(self.cursor[s] + k));
                }
            }
            self.rows.push(Row {
                kind,
                cells,
                emph_start: 0,
                emph_len: 0,
            });
        }

        for (cursor, count) in self.cursor[..sides].iter_mut().zip(&counts[..sides]) {
            *cursor += count;
        }
        start..start + height
    }

    /// Emit a block with an explicit per-row cell layout.
    ///
    /// The escape hatch for a smarter replace-pairing policy than index order: the caller
    /// supplies the rows, the builder still owns the cursor bookkeeping.
    ///
    /// # Panics
    /// If `pairs` does not consume each side's lines contiguously from the cursor.
    pub fn push_paired(&mut self, kind: RowKind, pairs: &[[Cell; MAX_SIDES]]) -> Range<u32> {
        let sides = self.side_count();
        let start = self.rows.len() as u32;
        let mut consumed = [0u32; MAX_SIDES];

        for cells in pairs {
            for (s, ((cell, cursor), consumed)) in cells[..sides]
                .iter()
                .zip(&self.cursor[..sides])
                .zip(consumed[..sides].iter_mut())
                .enumerate()
            {
                if let Cell::Line(n) = cell {
                    assert_eq!(
                        n.0,
                        cursor + *consumed,
                        "push_paired: side {s} skipped a line"
                    );
                    *consumed += 1;
                }
            }
            self.rows.push(Row {
                kind,
                cells: *cells,
                emph_start: 0,
                emph_len: 0,
            });
        }

        for (cursor, count) in self.cursor[..sides].iter_mut().zip(&consumed[..sides]) {
            *cursor += count;
        }
        start..start + pairs.len() as u32
    }

    /// Which already-emitted row shows `line` on `side`.
    ///
    /// This is what lets inline emphasis be attached without the caller reimplementing
    /// whatever pairing policy the block used.
    pub fn row_of(&self, side: SideIdx, line: u32) -> Option<u32> {
        let s = side.idx();
        let i = self.rows.partition_point(|r| match r.cells[s] {
            Cell::Line(n) => n.0 < line,
            Cell::Filler => true,
        });
        (self.rows.get(i)?.cells[s] == Cell::Line(LineNo(line))).then_some(i as u32)
    }

    /// Record intra-line emphasis as (start, end) byte ranges into the stripped line text.
    pub fn set_emphasis(
        &mut self,
        row: u32,
        side: SideIdx,
        ranges: impl IntoIterator<Item = (u32, u32)>,
    ) {
        for (start, end) in ranges {
            if start < end {
                self.pending_emph.push((row, Emph { side, start, end }));
            }
        }
    }

    pub fn finish(mut self) -> Alignment {
        let sides = self.side_count();

        // Sort emphasis by row so each row's ranges form one contiguous slice.
        self.pending_emph.sort_by_key(|(row, _)| *row);
        let mut emph = Vec::with_capacity(self.pending_emph.len());
        let mut i = 0;
        while i < self.pending_emph.len() {
            let row = self.pending_emph[i].0;
            let start = emph.len();
            while i < self.pending_emph.len() && self.pending_emph[i].0 == row {
                emph.push(self.pending_emph[i].1);
                i += 1;
            }
            let len = emph.len() - start;
            if let Some(r) = self.rows.get_mut(row as usize) {
                r.emph_start = start as u32;
                r.emph_len = u16::try_from(len).unwrap_or(u16::MAX);
            }
        }

        // Group contiguous non-Equal rows into hunks.
        let mut hunks: Vec<Hunk> = Vec::new();
        let mut open: Option<u32> = None;
        for (i, row) in self.rows.iter().enumerate() {
            match (row.kind.is_equal(), open) {
                (false, None) => open = Some(i as u32),
                (true, Some(start)) => {
                    hunks.push(Hunk {
                        start,
                        end: i as u32,
                    });
                    open = None;
                }
                _ => {}
            }
        }
        if let Some(start) = open {
            hunks.push(Hunk {
                start,
                end: self.rows.len() as u32,
            });
        }

        // The module invariant, checked against real inputs on every debug run. This is
        // deliberately here rather than in a property test: it then also runs during every
        // other test in the crate, and during development, on real files.
        #[cfg(debug_assertions)]
        for s in 0..sides {
            let mut want = 0u32;
            for row in &self.rows {
                if let Cell::Line(n) = row.cells[s] {
                    debug_assert_eq!(n.0, want, "side {s}: line sequence broken");
                    want += 1;
                }
            }
            debug_assert_eq!(want, self.cursor[s], "side {s}: cursor disagrees with rows");
        }

        let mut line_counts = [0u32; MAX_SIDES];
        line_counts[..sides].copy_from_slice(&self.cursor[..sides]);

        Alignment {
            sides: self.sides,
            rows: self.rows,
            emph,
            hunks,
            line_counts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders an alignment as a compact assertable table: left line, right line, kind.
    ///
    /// Every alignment test is then one readable `assert_eq!`, with the expectation visible
    /// in the test source rather than in a snapshot file.
    fn dump(a: &Alignment) -> String {
        let mut out = String::new();
        for (i, row) in a.rows().iter().enumerate() {
            let cell = |c: Cell| match c.line() {
                Some(n) => format!("{:>3}", n.display()),
                None => "   ".to_string(),
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
            let emph = a.emphasis(i, SideIdx::LEFT).count() + a.emphasis(i, SideIdx::RIGHT).count();
            let mark = if emph > 0 { "*" } else { "" };
            out.push_str(&format!(
                "{}|{} {kind}{mark}\n",
                cell(row.cells[0]),
                cell(row.cells[1])
            ));
        }
        out
    }

    fn two(f: impl FnOnce(&mut AlignmentBuilder)) -> Alignment {
        let mut b = AlignmentBuilder::new(2);
        f(&mut b);
        b.finish()
    }

    #[test]
    fn equal_rows_advance_both_sides_together() {
        let a = two(|b| {
            b.push_equal(3);
        });
        assert_eq!(dump(&a), "  1|  1 =\n  2|  2 =\n  3|  3 =\n");
        assert!(a.is_identical());
        assert_eq!(a.line_count(SideIdx::LEFT), 3);
    }

    #[test]
    fn replace_block_pads_the_shorter_side() {
        // 3 lines on the left against 5 on the right: 5 rows, fillers on the left in 3..5.
        let a = two(|b| {
            b.push_block(RowKind::Changed, [3, 5, 0]);
        });
        assert_eq!(
            dump(&a),
            "  1|  1 ~\n  2|  2 ~\n  3|  3 ~\n   |  4 ~\n   |  5 ~\n"
        );
    }

    #[test]
    fn pure_delete_fills_the_right() {
        let a = two(|b| {
            b.push_block(RowKind::Unmatched, [4, 0, 0]);
        });
        assert_eq!(dump(&a), "  1|    -\n  2|    -\n  3|    -\n  4|    -\n");
        assert_eq!(a.stats().deleted, 4);
        assert_eq!(a.stats().inserted, 0);
    }

    #[test]
    fn pure_insert_fills_the_left() {
        let a = two(|b| {
            b.push_block(RowKind::Unmatched, [0, 2, 0]);
        });
        assert_eq!(dump(&a), "   |  1 +\n   |  2 +\n");
        assert_eq!(a.stats().inserted, 2);
    }

    #[test]
    fn insert_at_the_top_then_equal() {
        let a = two(|b| {
            b.push_block(RowKind::Unmatched, [0, 2, 0]);
            b.push_equal(2);
        });
        assert_eq!(dump(&a), "   |  1 +\n   |  2 +\n  1|  3 =\n  2|  4 =\n");
    }

    #[test]
    fn delete_at_the_bottom() {
        let a = two(|b| {
            b.push_equal(2);
            b.push_block(RowKind::Unmatched, [2, 0, 0]);
        });
        assert_eq!(dump(&a), "  1|  1 =\n  2|  2 =\n  3|    -\n  4|    -\n");
    }

    #[test]
    fn adjacent_replace_and_insert_form_one_hunk() {
        let a = two(|b| {
            b.push_equal(1);
            b.push_block(RowKind::Changed, [1, 1, 0]);
            b.push_block(RowKind::Unmatched, [0, 1, 0]);
            b.push_equal(1);
        });
        assert_eq!(dump(&a), "  1|  1 =\n  2|  2 ~\n   |  3 +\n  3|  4 =\n");
        assert_eq!(a.hunks(), &[Hunk { start: 1, end: 3 }]);
    }

    #[test]
    fn wholly_different_files_are_one_hunk() {
        let a = two(|b| {
            b.push_block(RowKind::Changed, [2, 3, 0]);
        });
        assert_eq!(a.hunks(), &[Hunk { start: 0, end: 3 }]);
        assert!(!a.is_identical());
    }

    #[test]
    fn hunk_navigation_at_both_ends() {
        let a = two(|b| {
            b.push_equal(1);
            b.push_block(RowKind::Changed, [1, 1, 0]);
            b.push_equal(1);
            b.push_block(RowKind::Unmatched, [1, 0, 0]);
        });
        assert_eq!(a.hunks().len(), 2);
        assert_eq!(a.hunk_at_or_after(0), Some(Hunk { start: 1, end: 2 }));
        assert_eq!(a.hunk_at_or_after(1), Some(Hunk { start: 1, end: 2 }));
        assert_eq!(a.hunk_at_or_after(2), Some(Hunk { start: 3, end: 4 }));
        assert_eq!(a.hunk_at_or_after(4), None);
        assert_eq!(a.hunk_before(0), None);
        assert_eq!(a.hunk_before(2), Some(Hunk { start: 1, end: 2 }));
        assert_eq!(a.hunk_before(99), Some(Hunk { start: 3, end: 4 }));
    }

    #[test]
    fn row_of_line_finds_lines_after_a_filler_run() {
        // Left: 1 equal, 2 deleted, 1 equal. Right has a filler run in rows 1..3.
        let a = two(|b| {
            b.push_equal(1);
            b.push_block(RowKind::Unmatched, [2, 0, 0]);
            b.push_equal(1);
        });
        assert_eq!(dump(&a), "  1|  1 =\n  2|    -\n  3|    -\n  4|  2 =\n");

        assert_eq!(a.row_of_line(SideIdx::LEFT, LineNo(0)), Some(0));
        assert_eq!(a.row_of_line(SideIdx::LEFT, LineNo(3)), Some(3));
        // Right line 1 sits after the filler run, at row 3.
        assert_eq!(a.row_of_line(SideIdx::RIGHT, LineNo(1)), Some(3));
        assert_eq!(a.row_of_line(SideIdx::RIGHT, LineNo(2)), None);
    }

    #[test]
    fn empty_alignment_is_well_behaved() {
        let a = two(|_| {});
        assert!(a.is_empty());
        assert!(a.is_identical());
        assert_eq!(a.hunk_at_or_after(0), None);
        assert_eq!(a.hunk_before(0), None);
        assert_eq!(a.row_of_line(SideIdx::LEFT, LineNo(0)), None);
        assert_eq!(a.cell(0, SideIdx::LEFT), Cell::Filler);
    }

    #[test]
    fn emphasis_is_attached_per_row_and_side() {
        let mut b = AlignmentBuilder::new(2);
        b.push_equal(1);
        let block = b.push_block(RowKind::Changed, [2, 2, 0]);
        // Set out of order and across sides: finish() must still group them per row.
        b.set_emphasis(block.end - 1, SideIdx::RIGHT, [(0, 3)]);
        b.set_emphasis(block.start, SideIdx::LEFT, [(4, 9)]);
        b.set_emphasis(block.start, SideIdx::RIGHT, [(4, 7)]);
        // Empty ranges are dropped rather than stored.
        b.set_emphasis(block.start, SideIdx::LEFT, [(2, 2)]);
        let a = b.finish();

        assert_eq!(dump(&a), "  1|  1 =\n  2|  2 ~*\n  3|  3 ~*\n");

        let left: Vec<_> = a.emphasis(1, SideIdx::LEFT).copied().collect();
        assert_eq!(
            left,
            vec![Emph {
                side: SideIdx::LEFT,
                start: 4,
                end: 9
            }]
        );
        assert_eq!(a.emphasis(1, SideIdx::RIGHT).count(), 1);
        assert_eq!(a.emphasis(2, SideIdx::RIGHT).count(), 1);
        assert_eq!(a.emphasis(2, SideIdx::LEFT).count(), 0);
        assert_eq!(a.emphasis(0, SideIdx::LEFT).count(), 0);
    }

    #[test]
    fn push_paired_allows_a_custom_pairing() {
        let mut b = AlignmentBuilder::new(2);
        // Pair left line 0 with right line 1, leaving right line 0 unmatched above it.
        b.push_paired(
            RowKind::Changed,
            &[
                [Cell::Filler, Cell::Line(LineNo(0)), Cell::Filler],
                [Cell::Line(LineNo(0)), Cell::Line(LineNo(1)), Cell::Filler],
            ],
        );
        let a = b.finish();
        assert_eq!(dump(&a), "   |  1 ~\n  1|  2 ~\n");
        assert_eq!(a.line_count(SideIdx::LEFT), 1);
        assert_eq!(a.line_count(SideIdx::RIGHT), 2);
    }
}

// ------------------------------------------------------------------------------------
// Folding
// ------------------------------------------------------------------------------------
//
// Collapsing long identical stretches is a *view* transform: it produces a separate list
// of view rows and never touches the `Alignment`, which stays a pure function of its
// inputs. That is what lets folding be toggled at runtime without recomputing the diff.

/// One row as the panes see it: either an aligned row, or a collapsed run of equal ones.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewRow {
    Row(u32),
    /// A collapsed run, covering `start..end` of [`Alignment::rows`].
    Fold {
        start: u32,
        end: u32,
    },
}

impl ViewRow {
    /// The aligned row this view row starts at — where the cursor lands, and what a fold
    /// toggle uses to keep its place.
    pub const fn anchor(self) -> u32 {
        match self {
            Self::Row(i) => i,
            Self::Fold { start, .. } => start,
        }
    }

    /// How many aligned rows this hides. Zero for a plain row.
    pub const fn hidden(self) -> u32 {
        match self {
            Self::Row(_) => 0,
            Self::Fold { start, end } => end - start,
        }
    }

    pub const fn is_fold(self) -> bool {
        matches!(self, Self::Fold { .. })
    }
}

/// Every row, unfolded. The identity transform.
pub fn unfolded(alignment: &Alignment) -> Vec<ViewRow> {
    (0..alignment.len() as u32).map(ViewRow::Row).collect()
}

/// Collapse runs of equal rows, keeping `context` rows on each side of every hunk.
///
/// A run at the very start or end of the file needs context on one side only, since there
/// is no hunk beyond it — the same rule `diff -U` follows. A run is left alone unless
/// folding it would actually hide something: replacing one row with a one-row summary
/// costs content and saves nothing.
pub fn fold(alignment: &Alignment, context: u32) -> Vec<ViewRow> {
    let rows = alignment.rows();
    let total = rows.len();
    let context = context as usize;
    let mut out = Vec::new();
    let mut i = 0;

    while i < total {
        if !rows[i].kind.is_equal() {
            out.push(ViewRow::Row(i as u32));
            i += 1;
            continue;
        }

        // The maximal run of equal rows starting at `i`.
        let mut end = i;
        while end < total && rows[end].kind.is_equal() {
            end += 1;
        }

        let lead = if i == 0 { 0 } else { context };
        let trail = if end == total { 0 } else { context };
        let run = end - i;

        // Fold only when at least two rows disappear.
        if run > lead + trail + 1 {
            out.extend((i..i + lead).map(|k| ViewRow::Row(k as u32)));
            out.push(ViewRow::Fold {
                start: (i + lead) as u32,
                end: (end - trail) as u32,
            });
            out.extend((end - trail..end).map(|k| ViewRow::Row(k as u32)));
        } else {
            out.extend((i..end).map(|k| ViewRow::Row(k as u32)));
        }
        i = end;
    }

    out
}

/// Which view row shows `alignment_row`, or the fold that swallowed it.
///
/// View rows are ordered and non-overlapping, so this is a binary search rather than a
/// lookup table.
pub fn view_index_of(rows: &[ViewRow], alignment_row: u32) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let i = rows.partition_point(|r| match *r {
        ViewRow::Row(n) => n < alignment_row,
        ViewRow::Fold { end, .. } => end <= alignment_row,
    });
    Some(i.min(rows.len() - 1))
}

#[cfg(test)]
mod fold_tests {
    use super::*;

    /// Build an alignment from a pattern: '=' equal, 'x' changed.
    fn alignment(pattern: &str) -> Alignment {
        let mut b = AlignmentBuilder::new(2);
        for c in pattern.chars() {
            match c {
                '=' => {
                    b.push_equal(1);
                }
                _ => {
                    b.push_block(RowKind::Changed, [1, 1, 0]);
                }
            }
        }
        b.finish()
    }

    /// Render view rows compactly: a digit per kept row, `[n]` for a fold hiding n rows.
    fn dump(rows: &[ViewRow]) -> String {
        rows.iter()
            .map(|r| match *r {
                ViewRow::Row(i) => format!("{i} "),
                ViewRow::Fold { start, end } => format!("[{}] ", end - start),
            })
            .collect()
    }

    #[test]
    fn a_short_equal_run_is_left_alone() {
        // Nothing to gain: folding 1 row into a 1-row summary hides content for free.
        let a = alignment("x==x");
        assert_eq!(dump(&fold(&a, 3)), "0 1 2 3 ");
    }

    #[test]
    fn a_long_middle_run_collapses_keeping_context_on_both_sides() {
        let a = alignment("x==========x");
        // 1 changed, 10 equal, 1 changed; context 2 keeps 2 either side, hides 6.
        assert_eq!(dump(&fold(&a, 2)), "0 1 2 [6] 9 10 11 ");
    }

    #[test]
    fn a_leading_run_needs_context_on_one_side_only() {
        // Nothing precedes the file, so the head of the run is not context for anything.
        let a = alignment("=====x");
        assert_eq!(dump(&fold(&a, 2)), "[3] 3 4 5 ");
    }

    #[test]
    fn a_trailing_run_needs_context_on_one_side_only() {
        let a = alignment("x=====");
        assert_eq!(dump(&fold(&a, 2)), "0 1 2 [3] ");
    }

    #[test]
    fn a_wholly_identical_file_collapses_to_a_single_fold() {
        let a = alignment("========");
        assert_eq!(dump(&fold(&a, 3)), "[8] ");
    }

    #[test]
    fn zero_context_still_keeps_the_changed_rows() {
        let a = alignment("x=====x");
        assert_eq!(dump(&fold(&a, 0)), "0 [5] 6 ");
    }

    #[test]
    fn a_file_with_no_equal_rows_is_unchanged() {
        let a = alignment("xxx");
        assert_eq!(dump(&fold(&a, 3)), "0 1 2 ");
    }

    #[test]
    fn an_empty_alignment_folds_to_nothing() {
        let a = alignment("");
        assert!(fold(&a, 3).is_empty());
        assert!(unfolded(&a).is_empty());
        assert_eq!(view_index_of(&[], 0), None);
    }

    #[test]
    fn folding_never_loses_or_reorders_rows() {
        let a = alignment("x===========x==x=======");
        let rows = fold(&a, 2);
        let mut covered = Vec::new();
        for r in &rows {
            match *r {
                ViewRow::Row(i) => covered.push(i),
                ViewRow::Fold { start, end } => covered.extend(start..end),
            }
        }
        assert_eq!(
            covered,
            (0..a.len() as u32).collect::<Vec<_>>(),
            "the view must cover every aligned row exactly once, in order"
        );
    }

    #[test]
    fn view_index_finds_kept_rows_and_the_fold_that_hides_the_rest() {
        let a = alignment("x==========x");
        let rows = fold(&a, 2);
        // 0 1 2 [3..9] 9 10 11
        assert_eq!(view_index_of(&rows, 0), Some(0));
        assert_eq!(view_index_of(&rows, 2), Some(2));
        // A hidden row resolves to the fold standing in for it.
        assert_eq!(view_index_of(&rows, 5), Some(3));
        assert!(rows[3].is_fold());
        assert_eq!(view_index_of(&rows, 9), Some(4));
        assert_eq!(view_index_of(&rows, 11), Some(6));
    }

    #[test]
    fn the_unfolded_view_is_the_identity() {
        let a = alignment("x==x");
        let rows = unfolded(&a);
        assert_eq!(rows.len(), a.len());
        assert!(rows.iter().all(|r| !r.is_fold()));
        assert_eq!(view_index_of(&rows, 2), Some(2));
    }
}
