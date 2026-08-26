//! Colours. Kept in one place because this is the file edited constantly while tuning.

use ratatui::style::{Color, Modifier, Style};

use crate::model::{Cell, RowKind};

/// Background of the selected row when the row has no colour of its own.
const CURSOR_BG: Color = Color::Rgb(52, 52, 64);

/// How much to lift a row background to mark it as selected.
const CURSOR_LIFT: u8 = 38;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    /// No colours available, so the cursor has to be drawn with an attribute instead.
    pub monochrome: bool,
    pub equal: Style,
    pub changed: Style,
    pub added: Style,
    pub removed: Style,
    pub filler: Style,
    pub emph_added: Style,
    pub emph_removed: Style,
    pub gutter: Style,
    pub gutter_active: Style,
    pub header: Style,
    pub header_focus: Style,
    pub status: Style,
    pub marker: Style,
    pub fold: Style,
    pub search: Style,
    pub search_current: Style,
    pub warning: Style,
    pub identical: Style,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            monochrome: false,
            equal: Style::new(),
            changed: Style::new().bg(Color::Rgb(30, 40, 60)),
            added: Style::new().bg(Color::Rgb(20, 55, 30)),
            removed: Style::new().bg(Color::Rgb(60, 25, 30)),
            filler: Style::new().bg(Color::Rgb(24, 24, 28)),
            emph_added: Style::new().bg(Color::Rgb(35, 100, 45)),
            emph_removed: Style::new().bg(Color::Rgb(110, 40, 45)),
            gutter: Style::new().fg(Color::DarkGray),
            gutter_active: Style::new().fg(Color::Gray),
            header: Style::new().fg(Color::Gray),
            header_focus: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            status: Style::new().fg(Color::Gray),
            marker: Style::new().fg(Color::DarkGray),
            fold: Style::new()
                .fg(Color::DarkGray)
                .bg(Color::Rgb(22, 22, 26))
                .add_modifier(Modifier::ITALIC),
            search: Style::new().bg(Color::Rgb(90, 80, 20)).fg(Color::White),
            search_current: Style::new()
                .bg(Color::Rgb(200, 160, 40))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            warning: Style::new().fg(Color::Yellow),
            identical: Style::new()
                .fg(Color::Rgb(120, 220, 140))
                .add_modifier(Modifier::BOLD),
        }
    }

    /// Monochrome. The marker column carries all the meaning, so a `NO_COLOR` run stays
    /// legible rather than merely colourless.
    pub fn no_color() -> Self {
        let plain = Style::new();
        Self {
            monochrome: true,
            equal: plain,
            changed: plain,
            added: plain,
            removed: plain,
            filler: plain,
            emph_added: plain.add_modifier(Modifier::REVERSED),
            emph_removed: plain.add_modifier(Modifier::REVERSED),
            gutter: plain.add_modifier(Modifier::DIM),
            gutter_active: plain,
            header: plain,
            header_focus: plain.add_modifier(Modifier::BOLD),
            status: plain,
            marker: plain,
            fold: plain.add_modifier(Modifier::DIM),
            search: plain.add_modifier(Modifier::REVERSED),
            search_current: plain
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
            warning: plain.add_modifier(Modifier::BOLD),
            identical: plain.add_modifier(Modifier::BOLD),
        }
    }

    /// The background for one side of one row.
    ///
    /// Derived from the row kind plus whether *this* side is filler — never from a
    /// left/right field, so a third column needs no new code here.
    pub fn row_style(&self, kind: RowKind, cell: Cell, side_is_left: bool) -> Style {
        match kind {
            RowKind::Equal => self.equal,
            RowKind::Changed | RowKind::Conflict => {
                if cell.is_filler() {
                    self.filler
                } else {
                    self.changed
                }
            }
            RowKind::Unmatched => {
                if cell.is_filler() {
                    self.filler
                } else if side_is_left {
                    self.removed
                } else {
                    self.added
                }
            }
        }
    }

    /// The selected row, keeping whatever the row already meant.
    ///
    /// Deliberately not `REVERSED`: swapping foreground and background on a row that is
    /// already green or red destroys the very signal the colour carries. Lifting the
    /// row's own background keeps "this line was added" readable while still marking the
    /// cursor across the full width.
    pub fn cursor(&self, base: Style) -> Style {
        if self.monochrome {
            return base.add_modifier(Modifier::REVERSED);
        }
        let bg = match base.bg {
            Some(Color::Rgb(r, g, b)) => Color::Rgb(
                r.saturating_add(CURSOR_LIFT),
                g.saturating_add(CURSOR_LIFT),
                b.saturating_add(CURSOR_LIFT),
            ),
            // An unstyled row (an equal line) has no hue to preserve.
            _ => CURSOR_BG,
        };
        base.bg(bg)
    }

    pub fn emph_style(&self, side_is_left: bool) -> Style {
        if side_is_left {
            self.emph_removed
        } else {
            self.emph_added
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, LineNo, RowKind};

    #[test]
    fn the_cursor_lifts_a_row_background_without_changing_its_hue() {
        let theme = Theme::dark();
        let added = theme.row_style(RowKind::Unmatched, Cell::Line(LineNo(0)), false);
        let selected = theme.cursor(added);

        let (Some(Color::Rgb(r0, g0, b0)), Some(Color::Rgb(r1, g1, b1))) = (added.bg, selected.bg)
        else {
            panic!("both should carry an rgb background");
        };
        // Brighter on every channel...
        assert!(r1 > r0 && g1 > g0 && b1 > b0);
        // ...and still recognisably green, which is the whole point of not using REVERSED:
        // a selected added line must still read as added.
        assert!(g1 > r1 && g1 > b1, "selected added row lost its hue");
    }

    #[test]
    fn added_and_removed_stay_distinguishable_under_the_cursor() {
        let theme = Theme::dark();
        let added = theme.cursor(theme.row_style(RowKind::Unmatched, Cell::Line(LineNo(0)), false));
        let removed =
            theme.cursor(theme.row_style(RowKind::Unmatched, Cell::Line(LineNo(0)), true));
        assert_ne!(added.bg, removed.bg);
    }

    #[test]
    fn an_equal_row_gets_a_neutral_cursor_background() {
        let theme = Theme::dark();
        let equal = theme.row_style(RowKind::Equal, Cell::Line(LineNo(0)), true);
        assert_eq!(equal.bg, None, "an equal row has no background of its own");
        assert_eq!(theme.cursor(equal).bg, Some(CURSOR_BG));
    }

    #[test]
    fn the_monochrome_theme_falls_back_to_reverse_video() {
        let theme = Theme::no_color();
        let selected = theme.cursor(theme.equal);
        assert!(selected.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(selected.bg, None, "no colour means no background to lift");
    }

    #[test]
    fn lifting_saturates_rather_than_wrapping() {
        let theme = Theme::dark();
        let bright = Style::new().bg(Color::Rgb(250, 250, 250));
        assert_eq!(theme.cursor(bright).bg, Some(Color::Rgb(255, 255, 255)));
    }
}
