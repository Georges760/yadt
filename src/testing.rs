//! A terminal, as far as the tests need one.
//!
//! A frame is rendered through blit exactly as a session renders it, and the bytes blit would
//! write to the terminal are read back into a grid. Tests assert on that grid rather than on
//! blit's cell buffer, which is private — and so they see what a terminal would show.

use std::time::Duration;

use blit::Frame;
use blit_tui::{RendererConfig, TuiContext, TuiRenderer, Ui};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::style::{Color, Modifier};

/// One cell as the terminal holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermCell {
    /// Empty for the second column of a wide glyph.
    pub symbol: String,
    pub pen: Pen,
}

/// The colours and attributes text is currently written in. `None` is the default colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pen {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

pub struct Screen {
    width: u16,
    height: u16,
    cells: Vec<TermCell>,
}

/// Render one frame of `build` on a `width` × `height` terminal.
pub fn paint(width: u16, height: u16, build: impl FnOnce(Ui<'_>)) -> Screen {
    let config = RendererConfig::new().columns(width).rows(height);
    let mut context = TuiContext::new(TuiRenderer::new(config));
    crate::ui::render_frame(&mut Frame::default(), &mut context, Duration::ZERO, build);
    // A fresh renderer writes every cell, so the output describes the whole screen.
    Screen::parse(width.max(1), height.max(1), context.renderer().output())
}

/// The app's frame, as `ui::draw` builds it.
pub fn render(app: &mut App, width: u16, height: u16) -> Screen {
    paint(width, height, |ui| crate::ui::draw(ui, app))
}

impl Screen {
    pub fn cell(&self, x: u16, y: u16) -> &TermCell {
        &self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)]
    }

    pub fn fg(&self, x: u16, y: u16) -> Option<Color> {
        self.cell(x, y).pen.fg
    }

    pub fn bg(&self, x: u16, y: u16) -> Option<Color> {
        self.cell(x, y).pen.bg
    }

    /// Each row's text, with trailing blanks trimmed.
    pub fn lines(&self) -> Vec<String> {
        (0..self.height)
            .map(|y| self.row(y).trim_end().to_string())
            .collect()
    }

    /// Every row in full, one per line.
    pub fn text(&self) -> String {
        (0..self.height)
            .map(|y| self.row(y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn row(&self, y: u16) -> String {
        (0..self.width)
            .map(|x| self.cell(x, y).symbol.as_str())
            .collect()
    }

    /// Replay what blit wrote: cursor moves, SGR, and text.
    fn parse(width: u16, height: u16, output: &[u8]) -> Self {
        let blank = TermCell {
            symbol: " ".to_string(),
            pen: Pen::default(),
        };
        let mut screen = Self {
            width,
            height,
            cells: vec![blank; usize::from(width) * usize::from(height)],
        };
        let mut rest = std::str::from_utf8(output).expect("blit writes UTF-8");
        let (mut x, mut y) = (0u16, 0u16);
        let mut pen = Pen::default();

        while !rest.is_empty() {
            if let Some(sequence) = rest.strip_prefix("\x1b[") {
                let end = sequence
                    .find(|c: char| c.is_ascii_alphabetic())
                    .expect("an escape sequence ends in a letter");
                let params = &sequence[..end];
                match &sequence[end..=end] {
                    "H" => {
                        let (row, column) = params.split_once(';').expect("row;column");
                        y = row.parse::<u16>().expect("a row") - 1;
                        x = column.parse::<u16>().expect("a column") - 1;
                    }
                    "m" => pen.apply(params),
                    other => panic!("unexpected escape sequence {params}{other}"),
                }
                rest = &sequence[end + 1..];
                continue;
            }

            let grapheme = rest.graphemes(true).next().expect("rest is not empty");
            rest = &rest[grapheme.len()..];
            let columns = UnicodeWidthStr::width(grapheme).max(1) as u16;
            for dx in 0..columns {
                if x + dx < width && y < height {
                    let index = usize::from(y) * usize::from(width) + usize::from(x + dx);
                    screen.cells[index] = TermCell {
                        symbol: if dx == 0 {
                            grapheme.to_string()
                        } else {
                            String::new()
                        },
                        pen,
                    };
                }
            }
            x += columns;
        }
        screen
    }
}

impl Pen {
    fn apply(&mut self, params: &str) {
        let codes: Vec<u8> = params
            .split(';')
            .map(|code| code.parse().unwrap_or(0))
            .collect();
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => *self = Self::default(),
                1 => self.modifiers = self.modifiers.union(Modifier::BOLD),
                2 => self.modifiers = self.modifiers.union(Modifier::DIM),
                3 => self.modifiers = self.modifiers.union(Modifier::ITALIC),
                7 => self.modifiers = self.modifiers.union(Modifier::REVERSED),
                code @ 30..=37 => self.fg = Some(Color::Indexed(code - 30)),
                code @ 90..=97 => self.fg = Some(Color::Indexed(code - 90 + 8)),
                code @ 40..=47 => self.bg = Some(Color::Indexed(code - 40)),
                code @ 100..=107 => self.bg = Some(Color::Indexed(code - 100 + 8)),
                39 => self.fg = None,
                49 => self.bg = None,
                target @ (38 | 48) => {
                    let color = match codes[i + 1] {
                        5 => {
                            i += 2;
                            Color::Indexed(codes[i])
                        }
                        2 => {
                            i += 4;
                            Color::Rgb(codes[i - 2], codes[i - 1], codes[i])
                        }
                        other => panic!("unexpected colour form {other}"),
                    };
                    if target == 38 {
                        self.fg = Some(color);
                    } else {
                        self.bg = Some(color);
                    }
                }
                other => panic!("unexpected SGR code {other}"),
            }
            i += 1;
        }
    }
}
