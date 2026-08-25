//! Colours. Kept in one place because this is the file edited constantly while tuning.

use ratatui::style::{Color, Modifier, Style};

use crate::model::{Cell, RowKind};

#[derive(Clone, Copy, Debug)]
pub struct Theme {
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
    pub warning: Style,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
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
            warning: Style::new().fg(Color::Yellow),
        }
    }

    /// Monochrome. The marker column carries all the meaning, so a `NO_COLOR` run stays
    /// legible rather than merely colourless.
    pub fn no_color() -> Self {
        let plain = Style::new();
        Self {
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
            warning: plain.add_modifier(Modifier::BOLD),
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

    pub fn emph_style(&self, side_is_left: bool) -> Style {
        if side_is_left {
            self.emph_removed
        } else {
            self.emph_added
        }
    }
}
