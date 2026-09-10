//! Comparing two binaries as hex dumps.
//!
//! This reuses the text machinery wholesale by treating a fixed-size chunk of bytes as a
//! "line": [`align_bytes`] produces an ordinary [`Alignment`], so hunk navigation,
//! folding, the cursor and the marker column all work with no extra code. Long identical
//! stretches of a firmware image collapse the same way identical source does.
//!
//! Alignment is by offset, not by content. A byte inserted near the start of a file shifts
//! everything after it, and every later row will read as changed — the same thing `cmp`
//! reports, and honest about what it is. Recovering from insertions would need a
//! content-defined chunking pass, which is a different tool.

use crate::canvas::Canvas;
use crate::model::{Alignment, AlignmentBuilder, MAX_SIDES, RowKind, SideIdx};
use crate::pane::Overlay;
use crate::style::Style;

/// Bytes per row. Eight keeps a side-by-side pair inside a normal terminal; sixteen is the
/// classic `hexdump -C` width and wants a wide one.
pub const DEFAULT_BYTES_PER_ROW: u32 = 8;

/// Rows are always a multiple of this, so offsets stay easy to read across widths.
pub const BYTES_PER_ROW_STEP: u32 = 8;

/// Beyond this a row is unreadable however wide the terminal is.
pub const MAX_BYTES_PER_ROW: u32 = 64;

/// Rows are only re-fitted live below this size. Above it the initial fit from the
/// terminal size stands, because re-aligning a large file takes long enough to feel.
pub const REFIT_SIZE_LIMIT: usize = 8 * 1024 * 1024;

/// Columns a pane leaves for the row itself, given the whole terminal width.
///
/// Used before the files are loaded, so it assumes the widest gutter it is likely to
/// need; the draw-time fit corrects it for anything small enough to re-align.
pub fn pane_row_width(terminal_width: u16, gutter: u16) -> u16 {
    // Two panes plus the three-column marker between them.
    terminal_width
        .saturating_sub(3)
        .saturating_div(2)
        .saturating_sub(gutter + 1)
}

/// The widest multiple of [`BYTES_PER_ROW_STEP`] whose row fits `available` columns.
///
/// Sized for the layout that keeps the ASCII pane -- `4n + 3` columns -- so widening the
/// terminal adds bytes rather than trailing space. When even the narrowest row will not
/// fit, the floor is returned and [`Layout::for_width`] drops the ASCII pane at render
/// time instead.
pub fn fit_bytes_per_row(available: u16) -> u32 {
    let fits = (available as u32).saturating_sub(3) / 4;
    (fits / BYTES_PER_ROW_STEP * BYTES_PER_ROW_STEP).clamp(BYTES_PER_ROW_STEP, MAX_BYTES_PER_ROW)
}

/// Build an alignment over two byte slices, one row per chunk.
///
/// Rows where both sides have bytes are `Equal` or `Changed`; past the end of the shorter
/// file they are `Unmatched`, so the longer side's tail renders against filler.
pub fn align_bytes(left: &[u8], right: &[u8], per_row: u32) -> Alignment {
    let per_row = per_row.max(1) as usize;
    let mut b = AlignmentBuilder::new(2);

    let rows_l = left.len().div_ceil(per_row);
    let rows_r = right.len().div_ceil(per_row);
    let common = rows_l.min(rows_r);

    let mut row = 0;
    while row < common {
        let (cl, cr) = (chunk(left, row, per_row), chunk(right, row, per_row));
        if cl == cr {
            // Coalesce the equal run so the alignment carries whole hunks, exactly as the
            // text path does.
            let start = row;
            while row < common && chunk(left, row, per_row) == chunk(right, row, per_row) {
                row += 1;
            }
            b.push_equal((row - start) as u32);
        } else {
            let block = b.push_block(RowKind::Changed, counts(1, 1));
            mark_differences(&mut b, block.start, cl, cr);
            row += 1;
        }
    }

    // Whatever the longer file has past the end of the shorter one.
    if rows_l > common {
        b.push_block(RowKind::Unmatched, counts((rows_l - common) as u32, 0));
    } else if rows_r > common {
        b.push_block(RowKind::Unmatched, counts(0, (rows_r - common) as u32));
    }

    b.finish()
}

fn chunk(bytes: &[u8], row: usize, per_row: usize) -> &[u8] {
    let start = row * per_row;
    &bytes[start..(start + per_row).min(bytes.len())]
}

fn counts(left: u32, right: u32) -> [u32; MAX_SIDES] {
    let mut c = [0u32; MAX_SIDES];
    c[SideIdx::LEFT.idx()] = left;
    c[SideIdx::RIGHT.idx()] = right;
    c
}

/// Record which byte positions differ, as emphasis on both sides.
///
/// Offsets are positions within the chunk, not columns: turning them into columns is the
/// renderer's job, and depends on whether the ASCII pane is showing.
fn mark_differences(b: &mut AlignmentBuilder, row: u32, left: &[u8], right: &[u8]) {
    let mut spans = Vec::new();
    for i in 0..left.len().max(right.len()) {
        if left.get(i) != right.get(i) {
            spans.push((i as u32, i as u32 + 1));
        }
    }
    b.set_emphasis(row, SideIdx::LEFT, spans.iter().copied());
    b.set_emphasis(row, SideIdx::RIGHT, spans);
}

/// Layout of one rendered hex row.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub per_row: u32,
    pub ascii: bool,
}

impl Layout {
    /// Widest layout that fits `width` columns, dropping the ASCII pane before giving up.
    pub fn for_width(per_row: u32, width: u16) -> Self {
        let per_row = per_row.max(1);
        let full = Self {
            per_row,
            ascii: true,
        };
        if full.width() <= width as usize {
            full
        } else {
            Self {
                per_row,
                ascii: false,
            }
        }
    }

    /// Columns the hex area occupies: two digits per byte, single-spaced.
    pub const fn hex_width(self) -> usize {
        (self.per_row as usize) * 3 - 1
    }

    pub const fn width(self) -> usize {
        if self.ascii {
            // hex + "  |" + ascii + "|"
            self.hex_width() + 3 + self.per_row as usize + 1
        } else {
            self.hex_width()
        }
    }

    /// Column range of byte `i` in the hex area.
    const fn hex_span(self, i: u32) -> (u32, u32) {
        let start = i * 3;
        (start, start + 2)
    }

    /// Column of byte `i` in the ASCII area.
    const fn ascii_col(self, i: u32) -> u32 {
        self.hex_width() as u32 + 3 + i
    }
}

/// Render one chunk as `de ad be ef  |....|`.
///
/// Short chunks are padded so every row is the same width and the two panes stay in step.
pub fn format_row(bytes: &[u8], layout: Layout) -> String {
    let per_row = layout.per_row as usize;
    let mut out = String::with_capacity(layout.width());

    for i in 0..per_row {
        if i > 0 {
            out.push(' ');
        }
        match bytes.get(i) {
            Some(b) => out.push_str(&format!("{b:02x}")),
            None => out.push_str("  "),
        }
    }

    if layout.ascii {
        out.push_str("  |");
        for i in 0..per_row {
            out.push(match bytes.get(i) {
                // Only printable ASCII; everything else is a dot, as hexdump does.
                Some(&b) if (0x20..0x7f).contains(&b) => b as char,
                Some(_) => '.',
                None => ' ',
            });
        }
        out.push('|');
    }
    out
}

/// Turn byte-position emphasis into column overlays for a rendered row.
pub fn overlays(
    positions: impl Iterator<Item = (u32, u32)>,
    layout: Layout,
    style: Style,
) -> Vec<Overlay> {
    let mut out = Vec::new();
    for (start, end) in positions {
        for i in start..end {
            if i >= layout.per_row {
                continue;
            }
            let (from, to) = layout.hex_span(i);
            out.push(Overlay::new(from, to, style));
            if layout.ascii {
                let col = layout.ascii_col(i);
                out.push(Overlay::new(col, col + 1, style));
            }
        }
    }
    out
}

/// The byte offset a row starts at, for the gutter.
pub const fn row_offset(row: u32, per_row: u32) -> u64 {
    row as u64 * per_row as u64
}

/// Width needed for the offset gutter of a file this big.
pub fn gutter_width(size: u64) -> u16 {
    let digits = format!("{size:x}").len().max(4);
    // Round up to an even number so offsets read as byte pairs.
    (digits + digits % 2) as u16
}

/// One side of a hex comparison.
///
/// Structurally the same job as [`crate::pane::DiffPane`], and it shares the row
/// bookkeeping, the cursor treatment and `draw_line`. What differs is only that a row is a
/// chunk of bytes rather than a line of text — and because a rendered hex row is pure
/// ASCII, byte offsets in it *are* column offsets, so `draw_line` needs no special case.
pub struct HexPane<'a> {
    pub alignment: &'a Alignment,
    pub rows: &'a [crate::model::ViewRow],
    pub bytes: &'a [u8],
    pub side: SideIdx,
    pub top: usize,
    pub cursor: usize,
    pub h_scroll: u16,
    pub gutter_width: u16,
    pub per_row: u32,
    pub theme: &'a crate::theme::Theme,
    pub focused: bool,
}

impl HexPane<'_> {
    /// Draw the pane into a canvas exactly its size.
    pub fn render(self, width: u16, height: u16) -> Canvas {
        use crate::model::{Cell, ViewRow};

        let mut canvas = Canvas::new(width, height);
        if width == 0 || height == 0 {
            return canvas;
        }
        let side_is_left = self.side == SideIdx::LEFT;
        let gutter = self.gutter_width.min(width);
        let text_x = gutter.saturating_add(1);
        let text_width = width.saturating_sub(gutter + 1);
        let layout = Layout::for_width(self.per_row, text_width);

        for y in 0..height {
            let view_index = self.top + y as usize;
            let Some(view_row) = self.rows.get(view_index).copied() else {
                break;
            };
            let on_cursor = view_index == self.cursor;

            let (cell, mut style) = match view_row {
                ViewRow::Fold { .. } => (Cell::Filler, self.theme.fold),
                ViewRow::Row(i) => {
                    let Some(row) = self.alignment.row(i as usize) else {
                        break;
                    };
                    let cell = row.cells[self.side.idx()];
                    (cell, self.theme.row_style(row.kind, cell, side_is_left))
                }
            };
            if on_cursor {
                style = self.theme.cursor(style);
            }

            canvas.fill(0, y, width, style);

            if let ViewRow::Fold { start, end } = view_row {
                let hidden = u64::from(end - start) * u64::from(self.per_row);
                let label = format!(
                    "{:>width$} \u{22ef} {hidden} identical bytes",
                    "",
                    width = gutter as usize
                );
                canvas.print(0, y, &label, width, style);
                continue;
            }

            if let Cell::Line(n) = cell {
                let offset = row_offset(n.0, self.per_row);
                let gutter_style = if self.focused {
                    self.theme.gutter_active
                } else {
                    self.theme.gutter
                };
                let label = format!("{:0>width$x}", offset, width = gutter as usize);
                canvas.print(0, y, &label, gutter, style.patch(gutter_style));

                if text_width > 0 {
                    let start = offset as usize;
                    let end = (start + self.per_row as usize).min(self.bytes.len());
                    let chunk = self.bytes.get(start..end).unwrap_or_default();
                    let text = format_row(chunk, layout);

                    let emph_style = {
                        let s = self.theme.emph_style(side_is_left);
                        if on_cursor { self.theme.cursor(s) } else { s }
                    };
                    let spans = overlays(
                        self.alignment
                            .emphasis(view_row.anchor() as usize, self.side)
                            .map(|e| (e.start, e.end)),
                        layout,
                        emph_style,
                    );

                    crate::pane::draw_line(
                        &mut canvas,
                        text_x,
                        y,
                        text_width,
                        &text,
                        self.h_scroll,
                        // No tabs in a hex dump, so the tab width is irrelevant.
                        1,
                        style,
                        &spans,
                    );
                }
            }
        }
        canvas
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cell;

    fn dump(a: &Alignment) -> String {
        a.rows()
            .iter()
            .map(|r| {
                let cell = |c: Cell| match c.line() {
                    Some(n) => format!("{:>2}", n.0),
                    None => "  ".to_string(),
                };
                let kind = match r.kind {
                    RowKind::Equal => '=',
                    RowKind::Changed => '~',
                    RowKind::Conflict => '!',
                    RowKind::Unmatched => {
                        if r.cells[0].is_filler() {
                            '+'
                        } else {
                            '-'
                        }
                    }
                };
                format!("{}|{} {kind}\n", cell(r.cells[0]), cell(r.cells[1]))
            })
            .collect()
    }

    #[test]
    fn identical_bytes_align_as_equal() {
        let a = align_bytes(b"abcdefgh", b"abcdefgh", 4);
        assert_eq!(dump(&a), " 0| 0 =\n 1| 1 =\n");
        assert!(a.is_identical());
    }

    #[test]
    fn a_differing_byte_marks_only_its_row() {
        let a = align_bytes(b"abcdefgh", b"abcXefgh", 4);
        assert_eq!(dump(&a), " 0| 0 ~\n 1| 1 =\n");
        assert_eq!(a.hunks().len(), 1);
    }

    #[test]
    fn emphasis_marks_exactly_the_differing_byte_positions() {
        let a = align_bytes(b"abcd", b"aXcY", 4);
        let spans: Vec<_> = a
            .emphasis(0, SideIdx::LEFT)
            .map(|e| (e.start, e.end))
            .collect();
        assert_eq!(spans, [(1, 2), (3, 4)]);
        // Both sides are marked, so the difference is visible in either pane.
        assert_eq!(a.emphasis(0, SideIdx::RIGHT).count(), 2);
    }

    #[test]
    fn a_longer_file_renders_its_tail_against_filler() {
        let a = align_bytes(b"abcdefgh", b"abcd", 4);
        assert_eq!(dump(&a), " 0| 0 =\n 1|   -\n");

        let a = align_bytes(b"abcd", b"abcdefgh", 4);
        assert_eq!(dump(&a), " 0| 0 =\n  | 1 +\n");
    }

    #[test]
    fn a_partial_final_chunk_still_gets_a_row() {
        let a = align_bytes(b"abcde", b"abcde", 4);
        assert_eq!(dump(&a), " 0| 0 =\n 1| 1 =\n");
    }

    #[test]
    fn a_difference_in_a_partial_final_chunk_is_caught() {
        let a = align_bytes(b"abcde", b"abcdX", 4);
        assert_eq!(dump(&a), " 0| 0 =\n 1| 1 ~\n");
        let spans: Vec<_> = a
            .emphasis(1, SideIdx::LEFT)
            .map(|e| (e.start, e.end))
            .collect();
        assert_eq!(spans, [(0, 1)]);
    }

    #[test]
    fn empty_files_produce_no_rows() {
        assert!(align_bytes(b"", b"", 4).is_empty());
    }

    #[test]
    fn one_empty_side_is_all_unmatched() {
        let a = align_bytes(b"abcd", b"", 4);
        assert_eq!(dump(&a), " 0|   -\n");
    }

    #[test]
    fn a_single_inserted_byte_shifts_everything_after_it() {
        // Documented behaviour, not a bug: alignment is by offset, so an insertion makes
        // every later row differ. `cmp` says the same.
        let a = align_bytes(b"abcdefgh", b"Xabcdefg", 4);
        assert!(!a.is_identical());
        assert_eq!(a.rows().iter().filter(|r| r.kind.is_equal()).count(), 0);
    }

    #[test]
    fn a_row_renders_as_hex_and_ascii() {
        let layout = Layout {
            per_row: 4,
            ascii: true,
        };
        assert_eq!(format_row(b"AB\x00\xff", layout), "41 42 00 ff  |AB..|");
        assert_eq!(format_row(b"AB\x00\xff", layout).len(), layout.width());
    }

    #[test]
    fn a_short_row_is_padded_so_the_panes_stay_in_step() {
        let layout = Layout {
            per_row: 4,
            ascii: true,
        };
        let full = format_row(b"abcd", layout);
        let short = format_row(b"ab", layout);
        assert_eq!(short, "61 62        |ab  |");
        assert_eq!(short.len(), full.len());
    }

    #[test]
    fn dropping_the_ascii_pane_narrows_the_row() {
        let wide = Layout {
            per_row: 8,
            ascii: true,
        };
        let narrow = Layout::for_width(8, 20);
        assert!(!narrow.ascii, "20 columns cannot hold hex plus ascii");
        assert!(narrow.width() < wide.width());
        assert_eq!(format_row(b"ab", narrow).len(), narrow.width());
    }

    #[test]
    fn a_wide_pane_keeps_the_ascii() {
        let layout = Layout::for_width(8, 60);
        assert!(layout.ascii);
    }

    #[test]
    fn overlays_land_on_the_right_columns() {
        let layout = Layout {
            per_row: 4,
            ascii: true,
        };
        let row = format_row(b"abcd", layout);
        let out = overlays([(1, 2)].into_iter(), layout, Style::new());

        // Byte 1 is the second hex pair and the second ascii character.
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].start, out[0].end), (3, 5));
        assert_eq!(&row[3..5], "62");
        assert_eq!(&row[out[1].start as usize..out[1].end as usize], "b");
    }

    #[test]
    fn overlays_skip_the_ascii_column_when_it_is_not_drawn() {
        let layout = Layout {
            per_row: 4,
            ascii: false,
        };
        let out = overlays([(0, 1)].into_iter(), layout, Style::new());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn overlays_ignore_positions_past_the_row() {
        let layout = Layout {
            per_row: 4,
            ascii: false,
        };
        assert!(overlays([(9, 10)].into_iter(), layout, Style::new()).is_empty());
    }

    #[test]
    fn the_row_width_grows_in_steps_of_eight() {
        // Whatever the terminal, a row is a multiple of eight so offsets stay readable.
        for available in 0..400u16 {
            let n = fit_bytes_per_row(available);
            assert_eq!(n % BYTES_PER_ROW_STEP, 0, "{available} columns gave {n}");
            assert!((BYTES_PER_ROW_STEP..=MAX_BYTES_PER_ROW).contains(&n));
        }
    }

    #[test]
    fn the_fitted_row_actually_fits() {
        // Above the floor, the chosen width must leave the ascii pane intact.
        for available in 35..400u16 {
            let n = fit_bytes_per_row(available);
            if n < MAX_BYTES_PER_ROW {
                let layout = Layout {
                    per_row: n,
                    ascii: true,
                };
                assert!(
                    layout.width() <= available as usize,
                    "{n} bytes needs {} columns but only {available} are free",
                    layout.width()
                );
            }
        }
    }

    #[test]
    fn widening_the_terminal_never_narrows_the_row() {
        let mut last = 0;
        for available in 0..400u16 {
            let n = fit_bytes_per_row(available);
            assert!(n >= last, "{available} columns went backwards");
            last = n;
        }
    }

    #[test]
    fn known_widths_pick_the_expected_row() {
        // 4n + 3 columns for the layout with ascii.
        assert_eq!(fit_bytes_per_row(0), 8, "the floor holds when nothing fits");
        assert_eq!(fit_bytes_per_row(34), 8);
        assert_eq!(fit_bytes_per_row(35), 8, "8 bytes needs exactly 35");
        assert_eq!(
            fit_bytes_per_row(66),
            8,
            "16 bytes needs 67, so 66 stays at 8"
        );
        assert_eq!(fit_bytes_per_row(67), 16);
        assert_eq!(fit_bytes_per_row(99), 24, "24 bytes needs 99");
        assert_eq!(fit_bytes_per_row(131), 32);
    }

    #[test]
    fn the_row_is_capped_however_wide_the_terminal() {
        assert_eq!(fit_bytes_per_row(u16::MAX), MAX_BYTES_PER_ROW);
    }

    #[test]
    fn the_gutter_is_wide_enough_for_the_largest_offset() {
        assert_eq!(gutter_width(0), 4);
        assert_eq!(gutter_width(0xffff), 4);
        assert_eq!(gutter_width(0x10000), 6);
        assert_eq!(row_offset(3, 16), 48);
    }
}
