//! Application state: the view stack, key handling, and the event loop.
//!
//! `Esc` pops a view; popping the last one quits. File mode simply starts with one `File`
//! on the stack and directory mode with one `Dir`, so both entry points share everything
//! below the stack.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::layout::Rect;

use crate::diff::{DiffOptions, FileComparison};
use crate::dirdiff::{DirEntryRow, EntryStatus, FsSource, ScanEvent, ScanHandle, spawn_scan};
use crate::model::SideIdx;
use crate::pane::MAX_H_SCROLL;
use crate::theme::Theme;
use crate::{Inputs, exit, ui};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub diff: DiffOptions,
    pub tab_width: u8,
    pub max_file_size: u64,
    pub follow_symlinks: bool,
}

/// One entry on the view stack.
pub enum View {
    File(Box<FileView>),
    Dir(Box<DirView>),
}

// ---------------------------------------------------------------------------------------
// File view
// ---------------------------------------------------------------------------------------

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
        self.top = self.top.min(rows.saturating_sub(height));

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
        self.cursor = (self.cursor as isize + delta).clamp(0, rows as isize - 1) as usize;
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
            self.top = self.cursor.saturating_sub(self.viewport as usize / 3);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Directory view
// ---------------------------------------------------------------------------------------

/// Which entries the tree shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DirFilter {
    #[default]
    All,
    /// Hide entries that are the same on both sides.
    HideIdentical,
    /// Only entries whose contents differ — not one-sided entries or type mismatches.
    DifferingOnly,
}

impl DirFilter {
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::HideIdentical,
            Self::HideIdentical => Self::DifferingOnly,
            Self::DifferingOnly => Self::All,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::HideIdentical => "changes",
            Self::DifferingOnly => "differing",
        }
    }

    const fn accepts(self, status: EntryStatus) -> bool {
        match self {
            Self::All => true,
            Self::HideIdentical => !status.is_identical(),
            Self::DifferingOnly => matches!(status, EntryStatus::Differing | EntryStatus::Pending),
        }
    }
}

pub struct DirView {
    pub roots: [PathBuf; 2],
    pub rows: Vec<DirEntryRow>,
    /// Where each entry lives in `rows`, so a roll-up `Update` can find its directory.
    index: HashMap<PathBuf, usize>,
    collapsed: HashSet<PathBuf>,
    pub filter: DirFilter,
    /// Index into `visible`, not into `rows`.
    pub cursor: usize,
    pub top: usize,
    pub viewport: u16,
    scan: Option<ScanHandle>,
    pub scanned: usize,
    pub differing: usize,
    pub done: bool,
    pub errors: Vec<String>,
    visible: Vec<usize>,
    dirty: bool,
}

impl DirView {
    pub fn new(left: PathBuf, right: PathBuf, follow_symlinks: bool) -> Self {
        let mut view = Self {
            roots: [left, right],
            rows: Vec::new(),
            index: HashMap::new(),
            collapsed: HashSet::new(),
            filter: DirFilter::All,
            cursor: 0,
            top: 0,
            viewport: 1,
            scan: None,
            scanned: 0,
            differing: 0,
            done: false,
            errors: Vec::new(),
            visible: Vec::new(),
            dirty: true,
        };
        view.start_scan(follow_symlinks);
        view
    }

    fn start_scan(&mut self, follow_symlinks: bool) {
        let mut source = FsSource::new(self.roots[0].clone(), self.roots[1].clone());
        source.follow_symlinks = follow_symlinks;
        self.scan = Some(spawn_scan(Box::new(source)));
    }

    pub fn rescan(&mut self, follow_symlinks: bool) {
        // Dropping the handle cancels the running walk and joins it.
        self.scan = None;
        self.rows.clear();
        self.index.clear();
        self.errors.clear();
        self.scanned = 0;
        self.differing = 0;
        self.done = false;
        self.dirty = true;
        self.start_scan(follow_symlinks);
    }

    /// Drain whatever the scan thread has produced since the last tick.
    pub fn tick(&mut self) {
        // Bounded per tick so a fast scan cannot starve the redraw.
        for _ in 0..4096 {
            let Some(scan) = self.scan.as_ref() else {
                return;
            };
            match scan.rx.try_recv() {
                Ok(event) => self.apply(event),
                Err(_) => break,
            }
        }
    }

    /// Fold one scan event into the view. Shared by `tick` and `wait_for_scan`, so the
    /// counters cannot drift between the interactive and `--quiet` paths.
    fn apply(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Entry(row) => {
                self.index.insert(row.rel.clone(), self.rows.len());
                if row.status.differs() {
                    self.differing += 1;
                }
                self.rows.push(*row);
                self.scanned += 1;
                self.dirty = true;
            }
            ScanEvent::Update { rel, status } => {
                if let Some(&i) = self.index.get(&rel) {
                    let was = self.rows[i].status;
                    if was.differs() != status.differs() {
                        if status.differs() {
                            self.differing += 1;
                        } else {
                            self.differing = self.differing.saturating_sub(1);
                        }
                    }
                    self.rows[i].status = status;
                    self.dirty = true;
                }
            }
            ScanEvent::Progress { entries } => self.scanned = entries.max(self.scanned),
            ScanEvent::Error { message, .. } => self.errors.push(message),
            ScanEvent::Done { .. } => {
                self.done = true;
                self.dirty = true;
            }
        }
    }

    /// Indices into `rows` that are currently eligible to be shown.
    pub fn visible(&mut self) -> &[usize] {
        self.ensure_visible();
        &self.visible
    }

    fn ensure_visible(&mut self) {
        if self.dirty {
            self.recompute_visible();
        }
    }

    fn recompute_visible(&mut self) {
        self.visible.clear();
        for (i, row) in self.rows.iter().enumerate() {
            if !self.filter.accepts(row.status) {
                continue;
            }
            // Hidden if any ancestor directory is collapsed.
            if row
                .rel
                .ancestors()
                .skip(1)
                .any(|a| self.collapsed.contains(a))
            {
                continue;
            }
            self.visible.push(i);
        }
        self.dirty = false;
    }

    pub fn selected(&mut self) -> Option<&DirEntryRow> {
        let cursor = self.cursor;
        let i = *self.visible().get(cursor)?;
        self.rows.get(i)
    }

    pub fn clamp(&mut self, viewport: Rect) {
        self.viewport = viewport.height.max(1);
        let count = self.visible().len();
        let height = self.viewport as usize;

        self.cursor = self.cursor.min(count.saturating_sub(1));
        self.top = self.top.min(count.saturating_sub(height));

        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + height {
            self.top = self.cursor + 1 - height;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.visible().len();
        if count == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
    }

    fn goto_diff(&mut self, forward: bool) {
        let count = self.visible().len();
        if count == 0 {
            return;
        }
        let range: Vec<usize> = if forward {
            (self.cursor + 1..count).collect()
        } else {
            (0..self.cursor).rev().collect()
        };
        for pos in range {
            let row = self.visible[pos];
            if self.rows[row].status.differs() {
                self.cursor = pos;
                return;
            }
        }
    }

    fn toggle_expand(&mut self) {
        let Some(row) = self.selected() else {
            return;
        };
        if !row.kind.is_dir() {
            return;
        }
        let rel = row.rel.clone();
        if !self.collapsed.remove(&rel) {
            self.collapsed.insert(rel);
        }
        self.dirty = true;
    }

    fn set_collapsed(&mut self, collapsed: bool) {
        let Some(row) = self.selected() else {
            return;
        };
        let (rel, is_dir) = (row.rel.clone(), row.kind.is_dir());
        if is_dir && collapsed {
            self.collapsed.insert(rel);
            self.dirty = true;
        } else if is_dir && !collapsed {
            self.collapsed.remove(&rel);
            self.dirty = true;
        } else if collapsed {
            // On a file, collapse jumps to the parent directory instead.
            let Some(parent) = rel.parent().map(Path::to_path_buf) else {
                return;
            };
            self.ensure_visible();
            if let Some(pos) = self
                .visible
                .iter()
                .position(|&i| self.rows[i].rel == parent)
            {
                self.cursor = pos;
            }
        }
    }

    pub fn is_collapsed(&self, rel: &Path) -> bool {
        self.collapsed.contains(rel)
    }

    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.dirty = true;
    }

    /// Block until the scan finishes.
    ///
    /// Only for `--quiet`, which has no event loop to tick from and must not report an
    /// exit code before the answer is known.
    pub fn wait_for_scan(&mut self) {
        while !self.done {
            let event = match self.scan.as_ref() {
                Some(scan) => scan.rx.recv(),
                None => return,
            };
            match event {
                Ok(event) => self.apply(event),
                Err(_) => {
                    self.done = true;
                    return;
                }
            }
        }
    }

    #[cfg(test)]
    pub fn stop_scan_for_test(&mut self) {
        self.scan = None;
    }

    #[cfg(test)]
    pub fn apply_for_test(&mut self, event: ScanEvent) {
        self.apply(event);
    }

    #[cfg(test)]
    pub fn collapse_for_test(&mut self, rel: &Path) {
        self.collapsed.insert(rel.to_path_buf());
        self.dirty = true;
    }

    #[cfg(test)]
    pub fn mark_dirty_for_test(&mut self) {
        self.dirty = true;
    }

    /// Absolute paths of the selected entry on both sides.
    pub fn selected_paths(&mut self) -> Option<(PathBuf, PathBuf)> {
        let row = self.selected()?;
        let rel = row.rel.clone();
        Some((self.roots[0].join(&rel), self.roots[1].join(&rel)))
    }
}

// ---------------------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------------------

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
            Inputs::Dirs { left, right } => {
                let view = DirView::new(left, right, opts.follow_symlinks);
                // Not known until the scan finishes; updated in `tick`.
                (vec![View::Dir(Box::new(view))], exit::IDENTICAL)
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

    /// Settle every pending scan, then report. For `--quiet`, which has no event loop.
    pub fn exit_code_blocking(&mut self) -> i32 {
        for view in &mut self.views {
            if let View::Dir(dir) = view {
                dir.wait_for_scan();
            }
        }
        self.tick();
        self.exit_code
    }

    pub fn active_file(&mut self) -> Option<&mut FileView> {
        match self.views.last_mut() {
            Some(View::File(v)) => Some(v),
            _ => None,
        }
    }

    pub fn active_dir(&mut self) -> Option<&mut DirView> {
        match self.views.last_mut() {
            Some(View::Dir(v)) => Some(v),
            _ => None,
        }
    }

    pub fn pop_view(&mut self) {
        if self.views.len() <= 1 {
            self.should_quit = true;
        } else {
            self.views.pop();
        }
    }

    /// Drain background work. Every directory view on the stack is ticked, not just the
    /// active one, so a scan keeps progressing while the user reads a file diff.
    pub fn tick(&mut self) {
        let mut differing = None;
        for view in &mut self.views {
            if let View::Dir(dir) = view {
                dir.tick();
                if dir.done {
                    differing = Some(dir.differing);
                }
            }
        }
        if let Some(differing) = differing {
            self.exit_code = if differing > 0 {
                exit::DIFFERENT
            } else {
                exit::IDENTICAL
            };
        }
    }

    /// Open the selected directory entry: a directory expands, a file opens a diff.
    fn open_selected(&mut self) {
        let (max_size, diff_opts) = (self.opts.max_file_size, self.opts.diff);
        let Some(dir) = self.active_dir() else {
            return;
        };
        let Some(row) = dir.selected() else {
            return;
        };
        let (status, is_dir) = (row.status, row.kind.is_dir());

        if is_dir {
            dir.toggle_expand();
            return;
        }
        match status {
            EntryStatus::OnlyIn(_) | EntryStatus::TypeMismatch | EntryStatus::Unreadable(_) => {
                self.status = status.describe();
                return;
            }
            _ => {}
        }

        let Some((left, right)) = dir.selected_paths() else {
            return;
        };
        let cmp = crate::diff::compare_files(&left, &right, max_size, &diff_opts);
        self.status.clear();
        self.views.push(View::File(Box::new(FileView::new(cmp))));
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

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

        match self.views.last() {
            Some(View::File(_)) => self.handle_file_key(key, ctrl),
            Some(View::Dir(_)) => self.handle_dir_key(key, ctrl),
            None => {}
        }
    }

    fn handle_file_key(&mut self, key: KeyEvent, ctrl: bool) {
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
            KeyCode::Char('H') => view.h_scroll = view.h_scroll.saturating_sub(8),
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

    fn handle_dir_key(&mut self, key: KeyEvent, ctrl: bool) {
        if key.code == KeyCode::Enter {
            self.open_selected();
            return;
        }
        if key.code == KeyCode::Char('r') {
            let follow = self.opts.follow_symlinks;
            if let Some(dir) = self.active_dir() {
                dir.rescan(follow);
            }
            return;
        }

        let page = self.active_dir().map_or(1, |v| v.viewport.max(1) as isize);
        let Some(view) = self.active_dir() else {
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
                view.cursor = view.visible().len().saturating_sub(1);
            }
            KeyCode::Char('n') => view.goto_diff(true),
            KeyCode::Char('p') => view.goto_diff(false),
            KeyCode::Char('l') | KeyCode::Right => view.set_collapsed(false),
            KeyCode::Char('h') | KeyCode::Left => view.set_collapsed(true),
            KeyCode::Char('f') => view.cycle_filter(),
            _ => {}
        }
    }
}

/// The event loop.
///
/// Redraws every iteration and blocks in `poll` otherwise, so an idle yadt costs nothing —
/// while a running directory scan still lands within one poll interval.
pub fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        app.tick();

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.handle_key(key);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::dirdiff::{DirEntryRow, EntryKind};

    fn row(rel: &str, depth: u16, kind: EntryKind, status: EntryStatus) -> DirEntryRow {
        DirEntryRow {
            rel: PathBuf::from(rel),
            depth,
            kind,
            kinds: [Some(kind), Some(kind), None],
            present: [true, true, false],
            status,
            sizes: [None; crate::model::MAX_SIDES],
        }
    }

    /// A view with no scan thread, populated directly.
    fn view(rows: Vec<DirEntryRow>) -> DirView {
        let mut v = DirView {
            roots: ["l".into(), "r".into()],
            rows: Vec::new(),
            index: HashMap::new(),
            collapsed: HashSet::new(),
            filter: DirFilter::All,
            cursor: 0,
            top: 0,
            viewport: 10,
            scan: None,
            scanned: 0,
            differing: 0,
            done: true,
            errors: Vec::new(),
            visible: Vec::new(),
            dirty: true,
        };
        for r in rows {
            v.index.insert(r.rel.clone(), v.rows.len());
            v.rows.push(r);
        }
        v
    }

    fn names(v: &mut DirView) -> Vec<String> {
        v.visible()
            .to_vec()
            .iter()
            .map(|&i| v.rows[i].rel.display().to_string())
            .collect()
    }

    fn sample() -> DirView {
        view(vec![
            row("same.txt", 0, EntryKind::File, EntryStatus::Identical),
            row("changed.txt", 0, EntryKind::File, EntryStatus::Differing),
            row(
                "gone.txt",
                0,
                EntryKind::File,
                EntryStatus::OnlyIn(SideIdx::LEFT),
            ),
            row("sub", 0, EntryKind::Dir, EntryStatus::Differing),
            row("sub/a.txt", 1, EntryKind::File, EntryStatus::Identical),
            row("sub/b.txt", 1, EntryKind::File, EntryStatus::Differing),
        ])
    }

    #[test]
    fn the_all_filter_shows_everything() {
        let mut v = sample();
        assert_eq!(
            names(&mut v),
            [
                "same.txt",
                "changed.txt",
                "gone.txt",
                "sub",
                "sub/a.txt",
                "sub/b.txt"
            ]
        );
    }

    #[test]
    fn hide_identical_keeps_one_sided_entries() {
        let mut v = sample();
        v.filter = DirFilter::HideIdentical;
        v.dirty = true;
        assert_eq!(
            names(&mut v),
            ["changed.txt", "gone.txt", "sub", "sub/b.txt"]
        );
    }

    #[test]
    fn differing_only_drops_one_sided_entries_too() {
        let mut v = sample();
        v.filter = DirFilter::DifferingOnly;
        v.dirty = true;
        assert_eq!(names(&mut v), ["changed.txt", "sub", "sub/b.txt"]);
    }

    #[test]
    fn collapsing_a_directory_hides_its_subtree() {
        let mut v = sample();
        v.collapsed.insert(PathBuf::from("sub"));
        v.dirty = true;
        // The directory itself stays visible; only its children go.
        assert_eq!(
            names(&mut v),
            ["same.txt", "changed.txt", "gone.txt", "sub"]
        );
    }

    #[test]
    fn an_identical_directory_hides_its_whole_subtree_under_a_filter() {
        let mut v = view(vec![
            row("quiet", 0, EntryKind::Dir, EntryStatus::Identical),
            row("quiet/a.txt", 1, EntryKind::File, EntryStatus::Identical),
            row("loud", 0, EntryKind::Dir, EntryStatus::Differing),
            row("loud/b.txt", 1, EntryKind::File, EntryStatus::Differing),
        ]);
        v.filter = DirFilter::HideIdentical;
        v.dirty = true;
        assert_eq!(names(&mut v), ["loud", "loud/b.txt"]);
    }

    #[test]
    fn next_difference_skips_identical_entries() {
        let mut v = sample();
        v.cursor = 0; // same.txt
        v.goto_diff(true);
        assert_eq!(v.selected().unwrap().rel, PathBuf::from("changed.txt"));
        v.goto_diff(true);
        assert_eq!(v.selected().unwrap().rel, PathBuf::from("gone.txt"));
        v.goto_diff(false);
        assert_eq!(v.selected().unwrap().rel, PathBuf::from("changed.txt"));
    }

    #[test]
    fn navigation_at_the_ends_stays_put() {
        let mut v = sample();
        v.move_cursor(-5);
        assert_eq!(v.cursor, 0);
        v.move_cursor(500);
        assert_eq!(v.cursor, 5);
        v.goto_diff(true);
        assert_eq!(v.cursor, 5, "no later difference: cursor should not move");
    }

    #[test]
    fn toggling_expand_only_applies_to_directories() {
        let mut v = sample();
        v.cursor = 0; // same.txt, a file
        v.toggle_expand();
        assert!(v.collapsed.is_empty());

        v.cursor = 3; // sub, a directory
        v.toggle_expand();
        assert!(v.is_collapsed(Path::new("sub")));
        v.toggle_expand();
        assert!(!v.is_collapsed(Path::new("sub")));
    }

    #[test]
    fn an_empty_view_is_navigable_without_panicking() {
        let mut v = view(vec![]);
        v.move_cursor(3);
        v.goto_diff(true);
        v.toggle_expand();
        assert!(v.selected().is_none());
        assert!(names(&mut v).is_empty());
    }

    #[test]
    fn roll_up_updates_adjust_the_differing_count() {
        let mut v = view(vec![]);
        v.apply(ScanEvent::Entry(Box::new(row(
            "d",
            0,
            EntryKind::Dir,
            EntryStatus::Pending,
        ))));
        assert_eq!(
            v.differing, 0,
            "a pending directory is not yet a difference"
        );

        v.apply(ScanEvent::Update {
            rel: PathBuf::from("d"),
            status: EntryStatus::Differing,
        });
        assert_eq!(v.differing, 1);

        // A roll-up that settles on identical must take the count back down.
        v.apply(ScanEvent::Update {
            rel: PathBuf::from("d"),
            status: EntryStatus::Identical,
        });
        assert_eq!(v.differing, 0);
    }
}
