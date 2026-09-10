//! Colours and text attributes, in yadt's own terms.
//!
//! Nothing here names the terminal library. [`crate::canvas`] is the one place a [`Style`]
//! becomes a terminal cell, which keeps the theme and the panes testable without a terminal,
//! and means a colour is tuned in the same vocabulary whatever ends up drawing it.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    /// The terminal's own default.
    #[default]
    Reset,
    /// A palette entry: 0–7 are the normal colours, 8–15 their bright variants.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    pub const BLACK: Self = Self::Indexed(0);
    pub const YELLOW: Self = Self::Indexed(3);
    pub const GRAY: Self = Self::Indexed(7);
    pub const DARK_GRAY: Self = Self::Indexed(8);
    pub const WHITE: Self = Self::Indexed(15);
}

/// A set of text attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifier(u8);

impl Modifier {
    pub const NONE: Self = Self(0);
    pub const BOLD: Self = Self(1 << 0);
    pub const DIM: Self = Self(1 << 1);
    pub const ITALIC: Self = Self(1 << 2);
    pub const REVERSED: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// How a run of cells is drawn.
///
/// A colour left as `None` takes whatever is beneath it when patched, and the terminal's
/// default when drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            modifiers: Modifier::NONE,
        }
    }

    pub const fn fg(mut self, color: Color) -> Self {
        self.fg = Some(color);
        self
    }

    pub const fn bg(mut self, color: Color) -> Self {
        self.bg = Some(color);
        self
    }

    pub const fn add_modifier(mut self, modifier: Modifier) -> Self {
        self.modifiers = self.modifiers.union(modifier);
        self
    }

    /// `other` laid over this one: its colours win where it sets them, and attributes add up.
    ///
    /// This is how emphasis and search hits sit on a row without losing the row's colour.
    pub fn patch(self, other: Self) -> Self {
        Self {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            modifiers: self.modifiers.union(other.modifiers),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patching_keeps_what_the_overlay_leaves_unset() {
        let row = Style::new()
            .bg(Color::Rgb(20, 55, 30))
            .add_modifier(Modifier::DIM);
        let hit = Style::new().fg(Color::WHITE).add_modifier(Modifier::BOLD);
        let both = row.patch(hit);

        assert_eq!(both.bg, row.bg, "the row colour shows through");
        assert_eq!(both.fg, Some(Color::WHITE));
        assert!(both.modifiers.contains(Modifier::DIM.union(Modifier::BOLD)));
    }
}
