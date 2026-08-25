//! The diff pane widget, and the one function that has to be exactly right.
//!
//! This is a custom [`Widget`] rather than a `Paragraph` for a concrete reason:
//! `Paragraph` cannot paint a row background across the full pane width (trailing space
//! keeps the default style), and solid full-width bands are *the* visual affordance of a
//! side-by-side diff. `Paragraph`'s vertical scroll is also `for _ in 0..scroll.y`, which
//! is O(total lines) per frame.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::model::{Alignment, Cell, Emph, SideIdx, ViewRow};
use crate::text::TextFile;
use crate::theme::Theme;

/// Shown in place of a wide grapheme that would straddle a clip boundary.
const CLIPPED: &str = "\u{203a}"; // ›
const CLIPPED_LEFT: &str = "\u{2039}"; // ‹

/// Upper bound on horizontal scrolling, so a minified 10 MB line cannot be scrolled into
/// a position that costs a full grapheme walk per frame forever.
pub const MAX_H_SCROLL: u16 = 10_000;

pub struct DiffPane<'a> {
    pub alignment: &'a Alignment,
    /// What to draw, in order. Usually one entry per aligned row, but a collapsed run of
    /// equal rows appears as a single [`ViewRow::Fold`].
    pub rows: &'a [ViewRow],
    pub text: &'a TextFile,
    pub side: SideIdx,
    /// First visible row. Shared with the other pane — this is what makes scrolling
    /// synchronized without any per-pane arithmetic.
    pub top: usize,
    pub cursor: usize,
    pub h_scroll: u16,
    pub gutter_width: u16,
    pub tab_width: u8,
    pub theme: &'a Theme,
    pub focused: bool,
}

impl Widget for DiffPane<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let side_is_left = self.side == SideIdx::LEFT;
        let gutter = self.gutter_width.min(area.width);
        let text_x = area.x.saturating_add(gutter).saturating_add(1);
        let text_width = area.width.saturating_sub(gutter + 1);

        for dy in 0..area.height {
            let view_index = self.top + dy as usize;
            let y = area.y + dy;
            let Some(view_row) = self.rows.get(view_index).copied() else {
                break;
            };
            // Not gated on `focused`: both panes render the same row index, so the cursor
            // belongs on both. Which side has focus is shown by the gutter instead.
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

            // Paint the whole row first so the background reaches the pane edge.
            buf.set_style(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                },
                style,
            );

            if let ViewRow::Fold { start, end } = view_row {
                // The summary stands in for the hidden rows, and says how many.
                let hidden = end - start;
                let label = format!(
                    "{:>width$} \u{22ef} {hidden} identical line{}",
                    "",
                    if hidden == 1 { "" } else { "s" },
                    width = gutter as usize,
                );
                buf.set_stringn(area.x, y, &label, area.width as usize, style);
                continue;
            }

            if let Cell::Line(n) = cell {
                let gutter_style = if self.focused {
                    self.theme.gutter_active
                } else {
                    self.theme.gutter
                };
                let label = format!("{:>width$}", n.display(), width = gutter as usize);
                buf.set_stringn(
                    area.x,
                    y,
                    &label,
                    gutter as usize,
                    style.patch(gutter_style),
                );

                if text_width > 0 {
                    let emph: Vec<Emph> = self
                        .alignment
                        .emphasis(view_row.anchor() as usize, self.side)
                        .copied()
                        .collect();
                    draw_line(
                        buf,
                        text_x,
                        y,
                        text_width,
                        self.text.line(n),
                        self.h_scroll,
                        self.tab_width,
                        style,
                        &emph,
                        {
                            let emph = style.patch(self.theme.emph_style(side_is_left));
                            if on_cursor {
                                self.theme.cursor(emph)
                            } else {
                                emph
                            }
                        },
                    );
                }
            }
        }
    }
}

/// Write `line` into `buf` at (`x`, `y`), clipped to `width` display columns, after
/// skipping `h_scroll` display columns from the left.
///
/// Tabs advance to the next multiple of `tab_width` counted **from the start of the line**,
/// not from `x` — otherwise the expansion shifts when the gutter width changes. `emph`
/// holds sorted, non-overlapping byte ranges into `line`; graphemes falling inside one get
/// `emph_style`.
///
/// Never walks past `h_scroll + width` columns, so a 10 MB single line costs O(width)
/// rather than O(line length).
///
/// Returns the display width the line would occupy in total up to the clip point.
#[allow(clippy::too_many_arguments)]
pub fn draw_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    line: &str,
    h_scroll: u16,
    tab_width: u8,
    base: Style,
    emph: &[Emph],
    emph_style: Style,
) -> u16 {
    if width == 0 {
        return 0;
    }
    let tab_width = tab_width.max(1) as usize;
    let start_col = h_scroll as usize;
    let end_col = start_col + width as usize;

    // Column position within the line, before h_scroll is applied.
    let mut col = 0usize;

    for (offset, grapheme) in line.grapheme_indices(true) {
        if col >= end_col {
            break;
        }
        let style = if in_emphasis(emph, offset) {
            emph_style
        } else {
            base
        };

        if grapheme == "\t" {
            let stop = col + (tab_width - col % tab_width);
            // Emit only the part of the expansion that is actually visible, so scrolling
            // into the middle of a tab shows partial padding rather than skipping it.
            let from = col.max(start_col);
            let to = stop.min(end_col);
            for c in from..to {
                put(buf, x, y, c - start_col, width, " ", style);
            }
            col = stop;
            continue;
        }

        let w = UnicodeWidthStr::width(grapheme);

        if w == 0 {
            // A combining mark belongs on the previous cell rather than consuming one.
            // A defective cluster at the very start of a line has no previous cell, so it
            // is dropped rather than silently corrupting the row.
            if col > start_col && col <= end_col {
                append(buf, x, y, col - start_col - 1, width, grapheme);
            }
            continue;
        }

        // Entirely scrolled off to the left.
        if col + w <= start_col {
            col += w;
            continue;
        }

        // A wide grapheme straddling either clip edge cannot be drawn as itself: ratatui's
        // wide-cell bookkeeping assumes the following cell exists.
        if col < start_col {
            put(buf, x, y, 0, width, CLIPPED_LEFT, style);
            col += w;
            continue;
        }
        if col + w > end_col {
            put(buf, x, y, col - start_col, width, CLIPPED, style);
            col += w;
            continue;
        }

        let symbol = if grapheme.chars().next().is_some_and(control_picture_needed) {
            control_picture(grapheme)
        } else {
            grapheme.to_string()
        };
        put(buf, x, y, col - start_col, width, &symbol, style);
        col += w;
    }

    col.saturating_sub(start_col).min(width as usize) as u16
}

fn in_emphasis(emph: &[Emph], offset: usize) -> bool {
    let offset = offset as u32;
    emph.iter().any(|e| offset >= e.start && offset < e.end)
}

/// Write one symbol at column `dx` within the pane, if that cell exists.
///
/// Always goes through `cell_mut`: indexing panics, and a zero-width rect is reachable on
/// a narrow terminal.
fn put(buf: &mut Buffer, x: u16, y: u16, dx: usize, width: u16, symbol: &str, style: Style) {
    if dx >= width as usize {
        return;
    }
    let Some(px) = x.checked_add(dx as u16) else {
        return;
    };
    if let Some(cell) = buf.cell_mut((px, y)) {
        cell.set_symbol(symbol);
        cell.set_style(style);
    }
}

/// Append a zero-width grapheme onto the symbol already in a cell.
fn append(buf: &mut Buffer, x: u16, y: u16, dx: usize, width: u16, grapheme: &str) {
    if dx >= width as usize {
        return;
    }
    let Some(px) = x.checked_add(dx as u16) else {
        return;
    };
    if let Some(cell) = buf.cell_mut((px, y)) {
        let combined = format!("{}{}", cell.symbol(), grapheme);
        cell.set_symbol(&combined);
    }
}

fn control_picture_needed(c: char) -> bool {
    c.is_control() && c != '\t'
}

/// Render a control character as its Unicode Control Picture, so it is visible instead of
/// silently mangling the row.
fn control_picture(grapheme: &str) -> String {
    match grapheme.chars().next() {
        Some(c) if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32)
            .map(String::from)
            .unwrap_or_else(|| "?".to_string()),
        Some('\u{7f}') => "\u{2421}".to_string(),
        _ => grapheme.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(line: &str, width: u16, h_scroll: u16, tab_width: u8) -> String {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, 1));
        draw_line(
            &mut buf,
            0,
            0,
            width,
            line,
            h_scroll,
            tab_width,
            Style::new(),
            &[],
            Style::new(),
        );
        row_text(&buf, width)
    }

    fn row_text(buf: &Buffer, width: u16) -> String {
        (0..width)
            .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
            .collect()
    }

    #[test]
    fn plain_text_is_written_verbatim() {
        assert_eq!(render("hello", 10, 0, 8), "hello     ");
    }

    #[test]
    fn text_is_clipped_to_the_pane_width() {
        assert_eq!(render("hello world", 5, 0, 8), "hello");
    }

    #[test]
    fn horizontal_scroll_skips_leading_columns() {
        assert_eq!(render("hello world", 5, 6, 8), "world");
    }

    #[test]
    fn tab_advances_to_the_next_stop_from_the_line_start() {
        // A tab at column 0 fills columns 0..8.
        assert_eq!(render("\tx", 10, 0, 8), "        x ");
        // A tab at column 3 fills only columns 3..8 — five spaces, not eight.
        assert_eq!(render("abc\tx", 10, 0, 8), "abc     x ");
    }

    #[test]
    fn scrolling_into_a_tab_emits_partial_padding() {
        // Columns 0..8 are the tab; scrolling to column 5 leaves three of its spaces.
        assert_eq!(render("\tx", 6, 5, 8), "   x  ");
    }

    #[test]
    fn tab_width_is_configurable() {
        assert_eq!(render("\tx", 8, 0, 4), "    x   ");
    }

    #[test]
    fn wide_graphemes_occupy_two_columns() {
        // Each CJK character is two cells wide; ratatui leaves the trailing cell empty.
        let mut buf = Buffer::empty(Rect::new(0, 0, 6, 1));
        draw_line(
            &mut buf,
            0,
            0,
            6,
            "日本",
            0,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "本");
    }

    #[test]
    fn a_wide_grapheme_clipped_at_the_right_edge_becomes_a_marker() {
        // Width 3: 日 fits at 0..2, 本 would need 2..4 and must not be drawn.
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        draw_line(
            &mut buf,
            0,
            0,
            3,
            "日本",
            0,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "日");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), CLIPPED);
    }

    #[test]
    fn a_wide_grapheme_straddling_the_scroll_edge_becomes_a_marker() {
        // Scrolled to column 1, which is the second half of 日.
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        draw_line(
            &mut buf,
            0,
            0,
            4,
            "日本",
            1,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), CLIPPED_LEFT);
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "本");
    }

    #[test]
    fn control_characters_render_as_pictures() {
        // A lone CR is not a line terminator, so it has to be visible.
        assert_eq!(render("a\rb", 4, 0, 8), "a\u{240d}b ");
    }

    #[test]
    fn combining_marks_attach_to_the_previous_cell() {
        // "e" followed by U+0301 is one grapheme cluster of width 1.
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        draw_line(
            &mut buf,
            0,
            0,
            3,
            "e\u{301}x",
            0,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "e\u{301}");
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "x");
    }

    #[test]
    fn emphasis_applies_to_the_right_byte_range() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 11, 1));
        let emph = [Emph {
            side: SideIdx::LEFT,
            start: 6,
            end: 11,
        }];
        let base = Style::new();
        let hot = Style::new().add_modifier(ratatui::style::Modifier::REVERSED);
        draw_line(&mut buf, 0, 0, 11, "hello world", 0, 8, base, &emph, hot);

        let reversed = |x: u16| {
            buf.cell((x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        };
        assert!(!reversed(0), "'h' should not be emphasised");
        assert!(!reversed(5), "the space should not be emphasised");
        assert!(reversed(6), "'w' should be emphasised");
        assert!(reversed(10), "'d' should be emphasised");
    }

    #[test]
    fn emphasis_survives_a_tab_expansion() {
        // "a\tbc": byte 2 is 'b'. After expansion 'b' lands at column 8, not column 2 —
        // this is the offset bug this whole function exists to prevent.
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
        let emph = [Emph {
            side: SideIdx::LEFT,
            start: 2,
            end: 3,
        }];
        let hot = Style::new().add_modifier(ratatui::style::Modifier::REVERSED);
        draw_line(&mut buf, 0, 0, 12, "a\tbc", 0, 8, Style::new(), &emph, hot);
        let reversed = |x: u16| {
            buf.cell((x, 0))
                .unwrap()
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::REVERSED)
        };
        assert_eq!(buf.cell((8, 0)).unwrap().symbol(), "b");
        assert!(reversed(8), "'b' at column 8 should be emphasised");
        assert!(!reversed(9), "'c' should not be");
        assert!(!reversed(0), "'a' should not be");
    }

    #[test]
    fn a_zero_width_pane_draws_nothing_and_does_not_panic() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 0, 1));
        let written = draw_line(
            &mut buf,
            0,
            0,
            0,
            "hello",
            0,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(written, 0);
    }

    #[test]
    fn drawing_past_the_buffer_edge_does_not_panic() {
        // The rect is narrower than the width the caller claims.
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        draw_line(
            &mut buf,
            0,
            0,
            40,
            "a long line of text",
            0,
            8,
            Style::new(),
            &[],
            Style::new(),
        );
        assert_eq!(row_text(&buf, 3), "a l");
    }
}
