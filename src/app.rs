//! Application state: the view stack, key handling, and the event loop.
//!
//! `Esc` pops a view; popping the last one quits. File mode simply starts with one `File`
//! on the stack, so both entry points share everything below it.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;

use crate::diff::{DiffOptions, FileComparison};
use crate::model::SideIdx;
use crate::pane::MAX_H_SCROLL;
use crate::theme::Theme;
use crate::{Inputs, exit, ui};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub diff: DiffOptions,
    pub tab_width: u8,
    pub max_file_size: u64,
}

/// One entry on the view stack.
pub enum View {
    File(Box<FileView>),
}

pub struct FileView {
    pub cmp: FileComparison,
    /// First visible row. One value, both panes — this *is* the synchronized scrolling.
    pub top: usize,
    pub cursor: usize,
    pub h_scroll: u16,
    /// Which side the status bar reports a line number for.
    pub focus: SideIdx,
    /// Viewport height, recorded during draw so paging can use it.
    pub viewport: u16,
}

impl FileView {
    pub fn new(cmp: FileComparison) -> Self {
        Self {
            cmp,
            top: 0,
            cursor: 0,
            h_scroll: 0,
            focus: SideIdx::LEFT,
            viewport: 1,
        }
    }

    pub fn row_count(&self) -> usize {
        self.cmp.alignment.as_ref().map_or(0, |a| a.len())
    }

    /// Clamp scroll and cursor against the current viewport.
    ///
    /// Called at the top of every draw rather than from a resize handler, so it also
    /// covers the first frame and any freshly pushed view.
    pub fn clamp(&mut self, viewport: Rect) {
        self.viewport = viewport.height.max(1);
        let rows = self.row_count();
        let height = self.viewport as usize;

        self.cursor = self.cursor.min(rows.saturating_sub(1));
        let max_top = rows.saturating_sub(height);
        self.top = self.top.min(max_top);

        // Keep the cursor inside the viewport.
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + height {
            self.top = self.cursor + 1 - height;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let rows = self.row_count();
        if rows == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, rows as isize - 1);
        self.cursor = next as usize;
    }

    fn goto_hunk(&mut self, forward: bool) {
        let Some(alignment) = self.cmp.alignment.as_ref() else {
            return;
        };
        let hunk = if forward {
            alignment.hunk_at_or_after(self.cursor + 1)
        } else {
            alignment.hunk_before(self.cursor)
        };
        if let Some(hunk) = hunk {
            self.cursor = hunk.start as usize;
            // Land the hunk a third of the way down rather than at the very top.
            let margin = self.viewport as usize / 3;
            self.top = self.cursor.saturating_sub(margin);
        }
    }
}

pub struct App {
    pub views: Vec<View>,
    pub opts: Options,
    pub theme: Theme,
    pub help_visible: bool,
    pub status: String,
    pub should_quit: bool,
    exit_code: i32,
}

impl App {
    pub fn new(inputs: Inputs, opts: Options, theme: Theme) -> Result<Self> {
        let (views, exit_code) = match inputs {
            Inputs::Files { left, right } => {
                let cmp = crate::diff::compare_files(&left, &right, opts.max_file_size, &opts.diff);
                let code = if cmp.failed {
                    exit::ERROR
                } else if cmp.differ {
                    exit::DIFFERENT
                } else {
                    exit::IDENTICAL
                };
                (vec![View::File(Box::new(FileView::new(cmp)))], code)
            }
            Inputs::Dirs { .. } => {
                anyhow::bail!("directory comparison is not implemented yet (milestone 2)")
            }
        };

        Ok(Self {
            views,
            opts,
            theme,
            help_visible: false,
            status: String::new(),
            should_quit: false,
            exit_code,
        })
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn active(&mut self) -> Option<&mut View> {
        self.views.last_mut()
    }

    pub fn active_file(&mut self) -> Option<&mut FileView> {
        match self.views.last_mut() {
            Some(View::File(v)) => Some(v),
            None => None,
        }
    }

    pub fn pop_view(&mut self) {
        if self.views.len() <= 1 {
            self.should_quit = true;
        } else {
            self.views.pop();
        }
    }

    /// Drain any background work. A no-op until the directory scan exists.
    pub fn tick(&mut self) {}

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Global keys first.
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.help_visible = !self.help_visible;
                return;
            }
            KeyCode::Esc => {
                if self.help_visible {
                    self.help_visible = false;
                } else {
                    self.pop_view();
                }
                return;
            }
            _ => {}
        }

        if self.help_visible {
            return;
        }

        let page = self.active_file().map_or(1, |v| v.viewport.max(1) as isize);
        let Some(view) = self.active_file() else {
            return;
        };

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => view.move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => view.move_cursor(-1),
            KeyCode::Char('d') if ctrl => view.move_cursor(page / 2),
            KeyCode::Char('u') if ctrl => view.move_cursor(-page / 2),
            KeyCode::PageDown | KeyCode::Char(' ') => view.move_cursor(page),
            KeyCode::PageUp | KeyCode::Char('b') => view.move_cursor(-page),
            KeyCode::Char('g') | KeyCode::Home => view.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => {
                view.cursor = view.row_count().saturating_sub(1);
            }
            KeyCode::Char('n') => view.goto_hunk(true),
            KeyCode::Char('p') => view.goto_hunk(false),
            KeyCode::Char('l') | KeyCode::Right => {
                view.h_scroll = view.h_scroll.saturating_add(1).min(MAX_H_SCROLL);
            }
            KeyCode::Char('h') | KeyCode::Left => {
                view.h_scroll = view.h_scroll.saturating_sub(1);
            }
            KeyCode::Char('L') => {
                view.h_scroll = view.h_scroll.saturating_add(8).min(MAX_H_SCROLL);
            }
            KeyCode::Char('H') => {
                view.h_scroll = view.h_scroll.saturating_sub(8);
            }
            KeyCode::Char('0') => view.h_scroll = 0,
            KeyCode::Tab => {
                view.focus = if view.focus == SideIdx::LEFT {
                    SideIdx::RIGHT
                } else {
                    SideIdx::LEFT
                };
            }
            _ => {}
        }
    }
}

/// The event loop.
///
/// Redraws on every iteration and blocks in `poll` otherwise, so an idle yadt costs
/// nothing.
pub fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        app.tick();

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }
    }
    Ok(())
}
