//! A region's cells, recorded while the frame is built and painted once blit has placed it.
//!
//! blit builds a frame in two passes. Widgets run first, with the application in reach, and
//! describe a tree; atoms paint afterwards, once layout is done — and an atom must be
//! `'static`, so it cannot borrow the alignment or the text it is meant to draw. A `Canvas`
//! sits between the two: the panes write into it while `App` is borrowed, and it replays
//! those writes into terminal cells at paint time. It only ever holds what is visible, so a
//! large file costs nothing extra per frame.
//!
//! This is also the one module that turns a [`Style`] into a blit cell style.

use blit::{Atom, AtomCx, Constraints, PhysicalRect, Size};
use blit_tui::Context;
use blit_tui::cell::{Cell, CellStyle};
use blit_tui::color::Color as TermColor;
use blit_tui::text::{Span, TextAttributes};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::style::{Color, Modifier, Style};

pub struct Canvas {
    width: u16,
    height: u16,
    /// Every symbol written, end to end. Ops index into it, so a frame costs a handful of
    /// allocations rather than one per cell.
    text: String,
    ops: Vec<Op>,
}

#[derive(Clone, Copy, Debug)]
enum Op {
    /// Blank a run of cells in one style.
    Fill {
        x: u16,
        y: u16,
        width: u16,
        style: Style,
    },
    /// One grapheme, which may be two columns wide.
    Symbol {
        x: u16,
        y: u16,
        start: u32,
        end: u32,
        style: Style,
    },
}

impl Canvas {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            text: String::new(),
            ops: Vec::new(),
        }
    }

    pub const fn width(&self) -> u16 {
        self.width
    }

    pub const fn height(&self) -> u16 {
        self.height
    }

    /// Blank `width` cells of row `y` from `x`, so a background reaches the edge.
    pub fn fill(&mut self, x: u16, y: u16, width: u16, style: Style) {
        if x >= self.width || y >= self.height {
            return;
        }
        let width = width.min(self.width - x);
        if width > 0 {
            self.ops.push(Op::Fill { x, y, width, style });
        }
    }

    /// Write one grapheme at (`x`, `y`).
    ///
    /// Anything off the canvas is dropped rather than rejected, which is what makes a
    /// zero-width pane on a narrow terminal harmless.
    pub fn put(&mut self, x: u16, y: u16, symbol: &str, style: Style) {
        if x >= self.width || y >= self.height || symbol.is_empty() {
            return;
        }
        let start = self.text.len() as u32;
        self.text.push_str(symbol);
        self.ops.push(Op::Symbol {
            x,
            y,
            start,
            end: self.text.len() as u32,
            style,
        });
    }

    /// Attach a zero-width grapheme to the symbol last written at (`x`, `y`), if there is one.
    pub fn append(&mut self, x: u16, y: u16, grapheme: &str) {
        let Some(&Op::Symbol {
            start, end, style, ..
        }) = self.ops.iter().rev().find(
            |op| matches!(op, Op::Symbol { x: at_x, y: at_y, .. } if *at_x == x && *at_y == y),
        )
        else {
            return;
        };
        let combined = format!("{}{grapheme}", &self.text[start as usize..end as usize]);
        self.put(x, y, &combined, style);
    }

    /// Write `text` from (`x`, `y`), stopping before the first grapheme that would take it
    /// past `max_width` columns. Returns the columns used.
    pub fn print(&mut self, x: u16, y: u16, text: &str, max_width: u16, style: Style) -> u16 {
        let mut used = 0u16;
        for grapheme in text.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme);
            if width == 0 {
                continue;
            }
            if usize::from(used) + width > usize::from(max_width) {
                break;
            }
            self.put(x.saturating_add(used), y, grapheme, style);
            used += width as u16;
        }
        used
    }
}

impl<C: Context> Atom<C> for Canvas {
    fn measure(&self, _: &mut AtomCx<'_, C>, constraints: Constraints<i32>) -> Size<i32> {
        constraints.constrain(Size::new(i32::from(self.width), i32::from(self.height)))
    }

    fn paint(&self, cx: &mut AtomCx<'_, C>, area: PhysicalRect) {
        let mut cells = cx.context_mut().as_mut().cells(area);
        for op in &self.ops {
            match *op {
                Op::Fill { x, y, width, style } => {
                    let blank = Cell::new(' ').style(cell_style(style));
                    for column in x..x + width {
                        cells.set_cell(column.into(), y.into(), blank);
                    }
                }
                Op::Symbol {
                    x,
                    y,
                    start,
                    end,
                    style,
                } => {
                    let symbol = &self.text[start as usize..end as usize];
                    let style = cell_style(style);
                    // A lone scalar goes straight into its cell. Only a cluster needs blit's
                    // text layout, which is what keeps its code points in one cell.
                    let mut chars = symbol.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => {
                            cells.set_cell(x.into(), y.into(), Cell::new(c).style(style));
                        }
                        _ => cells.write(x.into(), y.into(), symbol, style),
                    }
                }
            }
        }
    }
}

/// A span of blit text in a yadt style, for what is drawn with blit's own `Text`.
pub fn span(text: &str, style: Style) -> Span<'_> {
    let mut span = Span::new(text).attributes(attributes(style.modifiers));
    if let Some(fg) = style.fg {
        span = span.color(term_color(fg));
    }
    if let Some(bg) = style.bg {
        span = span.background(term_color(bg));
    }
    span
}

/// A cell style with every colour written out, `Reset` included, so a cell shows exactly
/// what the pane computed rather than whatever an earlier atom left beneath it.
fn cell_style(style: Style) -> CellStyle {
    CellStyle::new()
        .foreground(term_color(style.fg.unwrap_or_default()))
        .background(term_color(style.bg.unwrap_or_default()))
        .attributes(attributes(style.modifiers))
}

fn term_color(color: Color) -> TermColor {
    match color {
        Color::Reset => TermColor::Reset,
        Color::Indexed(index) => TermColor::Indexed(index),
        Color::Rgb(r, g, b) => TermColor::Rgb(r, g, b),
    }
}

fn attributes(modifiers: Modifier) -> TextAttributes {
    let mut out = TextAttributes::NONE;
    for (modifier, attribute) in [
        (Modifier::BOLD, TextAttributes::BOLD),
        (Modifier::DIM, TextAttributes::DIM),
        (Modifier::ITALIC, TextAttributes::ITALIC),
        (Modifier::REVERSED, TextAttributes::INVERSE),
    ] {
        if modifiers.contains(modifier) {
            out |= attribute;
        }
    }
    out
}

#[cfg(test)]
impl Canvas {
    /// The symbol and style each cell is left with once every op has run.
    fn resolve(&self) -> Vec<Vec<(String, Style)>> {
        let blank = (" ".to_string(), Style::new());
        let mut grid = vec![vec![blank; self.width as usize]; self.height as usize];
        for op in &self.ops {
            match *op {
                Op::Fill { x, y, width, style } => {
                    for column in x..x + width {
                        grid[y as usize][column as usize] = (" ".to_string(), style);
                    }
                }
                Op::Symbol {
                    x,
                    y,
                    start,
                    end,
                    style,
                } => {
                    let symbol = self.text[start as usize..end as usize].to_string();
                    grid[y as usize][x as usize] = (symbol, style);
                }
            }
        }
        grid
    }

    pub fn symbol(&self, x: u16, y: u16) -> String {
        self.resolve()[y as usize][x as usize].0.clone()
    }

    pub fn style(&self, x: u16, y: u16) -> Style {
        self.resolve()[y as usize][x as usize].1
    }
}

#[cfg(test)]
mod tests {
    use blit::PhysicalRect;
    use blit_tui::layout::rect;

    use super::*;
    use crate::testing;

    #[test]
    fn writes_off_the_canvas_are_dropped() {
        let mut canvas = Canvas::new(2, 1);
        canvas.put(2, 0, "x", Style::new());
        canvas.put(0, 1, "x", Style::new());
        canvas.fill(5, 0, 3, Style::new());
        assert!(canvas.ops.is_empty());
    }

    #[test]
    fn a_zero_width_grapheme_joins_the_cell_before_it() {
        let mut canvas = Canvas::new(3, 1);
        canvas.put(0, 0, "e", Style::new());
        canvas.append(0, 0, "\u{301}");
        assert_eq!(canvas.symbol(0, 0), "e\u{301}");
    }

    #[test]
    fn print_stops_before_a_grapheme_that_would_overflow() {
        let mut canvas = Canvas::new(8, 1);
        assert_eq!(canvas.print(0, 0, "a\u{65e5}b", 2, Style::new()), 1);
        assert_eq!(canvas.symbol(1, 0), " ");
    }

    #[test]
    fn painting_through_blit_keeps_clusters_wide_glyphs_and_bands() {
        let band = Style::new().bg(Color::Rgb(20, 55, 30));
        let mut canvas = Canvas::new(6, 1);
        canvas.fill(0, 0, 6, band);
        canvas.put(0, 0, "e\u{301}", band);
        canvas.put(1, 0, "\u{65e5}", band);
        canvas.put(3, 0, "x", band.fg(Color::YELLOW));

        let screen = testing::paint(6, 1, |ui| {
            ui.layout(rect::new())
                .child()
                .item(PhysicalRect::new(0, 0, 6, 1))
                .insert(canvas);
        });

        assert_eq!(screen.lines(), ["e\u{301}\u{65e5}x"]);
        // The band reaches the edge: under the glyphs, and past them.
        assert!(
            (0..6).all(|x| screen.bg(x, 0) == band.bg),
            "{:?}",
            (0..6).map(|x| screen.bg(x, 0)).collect::<Vec<_>>()
        );
        assert_eq!(screen.fg(3, 0), Some(Color::YELLOW));
    }
}
