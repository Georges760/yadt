//! Application state: the view stack, key handling, and the event loop.
//!
//! `Esc` pops a view; popping the last one quits. File mode simply starts with one `File`
//! on the stack and directory mode with one `Dir`, so both entry points share everything
//! below the stack.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use blit::{Frame, Input};
use blit_tui::{Session, Ui};

use crate::diff::{DiffOptions, FileComparison};
use crate::dirdiff::{DirEntryRow, EntryStatus, FsSource, ScanEvent, ScanHandle, spawn_scan};
use crate::edit::{self, Edit};
use crate::key::{KeyCode, KeyEvent};
use crate::model::{self, SideIdx, ViewRow};
use crate::pane::MAX_H_SCROLL;
use crate::search::{self, Match};
use crate::theme::Theme;
use crate::{Inputs, exit, ui};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub diff: DiffOptions,
    pub tab_width: u8,
    pub max_file_size: u64,
    pub follow_symlinks: bool,
    /// `Some(n)` starts with equal runs collapsed, keeping `n` lines of context.
    pub context: Option<u32>,
    /// `--hex-width` was given, so the row width must not be refitted to the terminal.
    pub hex_width_pinned: bool,
}

/// One entry on the view stack.
pub enum View {
    File(Box<FileView>),
    Dir(Box<DirView>),
}

// ---------------------------------------------------------------------------------------
// File view
// ---------------------------------------------------------------------------------------

/// Context lines kept either side of a hunk when folding is switched on at runtime.
pub const DEFAULT_CONTEXT: u32 = 3;

/// An active search: the query, every hit, and which one is selected.
pub struct SearchState {
    pub query: String,
    pub matches: Vec<Match>,
    pub current: usize,
}

impl SearchState {
    pub fn current_match(&self) -> Option<Match> {
        self.matches.get(self.current).copied()
    }
}

pub struct FileView {
    pub cmp: FileComparison,
    /// What the panes draw. One entry per aligned row when unfolded; a collapsed run of
    /// equal rows becomes a single entry.
    pub rows: Vec<ViewRow>,
    /// `Some(n)` collapses equal runs, keeping `n` rows of context. `None` shows every row.
    pub context: Option<u32>,
    /// Kept so an edit can re-diff with the same settings the comparison was made with.
    pub diff_opts: DiffOptions,
    /// Edits that can still be undone, most recent last.
    pub undo_log: Vec<Edit>,
    /// Which sides have unsaved changes.
    pub dirty: [bool; 2],
    /// The active search, if any.
    pub search: Option<SearchState>,
    /// Show the "identical" overlay until the first keystroke.
    pub notice: bool,
    /// True when `--hex-width` pinned the row width, so it must not be refitted.
    pub hex_pinned: bool,
    /// First visible view row. One value, both panes — this *is* the synchronized
    /// scrolling.
    pub top: usize,
    /// Cursor position, also a view-row index.
    pub cursor: usize,
    pub h_scroll: u16,
    /// Which side the status bar reports a line number for.
    pub focus: SideIdx,
    /// Viewport height, recorded during draw so paging can use it.
    pub viewport: u16,
}

impl FileView {
    pub fn new(cmp: FileComparison) -> Self {
        Self::with_context(cmp, None)
    }

    pub fn with_context(cmp: FileComparison, context: Option<u32>) -> Self {
        Self::configured(cmp, context, DiffOptions::default())
    }

    pub fn configured(cmp: FileComparison, context: Option<u32>, diff_opts: DiffOptions) -> Self {
        let (cmp_differs, cmp_failed) = (cmp.differ, cmp.failed);
        let mut view = Self {
            cmp,
            rows: Vec::new(),
            context,
            diff_opts,
            undo_log: Vec::new(),
            dirty: [false; 2],
            top: 0,
            cursor: 0,
            h_scroll: 0,
            focus: SideIdx::LEFT,
            viewport: 1,
            search: None,
            // Worth saying out loud: a screen of two identical files looks exactly like a
            // screen where the diff has not loaded yet.
            notice: !cmp_differs && !cmp_failed,
            hex_pinned: false,
        };
        view.rebuild_rows();
        view
    }

    /// Recompute the view rows from the alignment.
    ///
    /// The alignment itself is never touched — folding is a view transform — so this is
    /// cheap enough to run on every toggle without re-diffing.
    fn rebuild_rows(&mut self) {
        self.rows = match (self.cmp.alignment.as_ref(), self.context) {
            (Some(alignment), Some(context)) => model::fold(alignment, context),
            (Some(alignment), None) => model::unfolded(alignment),
            (None, _) => Vec::new(),
        };
    }

    /// Number of rows on screen, which is not the number of aligned rows once folded.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The aligned row the cursor sits on, for reporting line numbers.
    pub fn cursor_row(&self) -> Option<u32> {
        self.rows.get(self.cursor).map(|r| r.anchor())
    }

    pub fn cursor_is_fold(&self) -> bool {
        self.rows.get(self.cursor).is_some_and(|r| r.is_fold())
    }

    /// Total rows hidden by folds, for the status bar.
    pub fn hidden_rows(&self) -> u32 {
        self.rows.iter().map(|r| r.hidden()).sum()
    }

    /// Run a search and jump to the first hit at or after the cursor.
    pub fn run_search(&mut self, query: &str) {
        let Some((left, right)) = self.cmp.texts() else {
            return;
        };
        let Some(alignment) = self.cmp.alignment.as_ref() else {
            return;
        };
        let matches = search::find(alignment, [left, right], query);
        if matches.is_empty() {
            self.search = Some(SearchState {
                query: query.to_string(),
                matches,
                current: 0,
            });
            return;
        }
        let from = self.cursor_row().unwrap_or(0);
        let current = search::next_from(&matches, from, true).unwrap_or(0);
        self.search = Some(SearchState {
            query: query.to_string(),
            matches,
            current,
        });
        self.reveal_current_match();
    }

    pub fn clear_search(&mut self) {
        self.search = None;
    }

    /// Step to the next or previous hit, wrapping at the ends.
    pub fn step_match(&mut self, forward: bool) {
        let Some(state) = self.search.as_mut() else {
            return;
        };
        let count = state.matches.len();
        if count == 0 {
            return;
        }
        state.current = if forward {
            (state.current + 1) % count
        } else {
            (state.current + count - 1) % count
        };
        self.reveal_current_match();
    }

    /// Scroll so the selected hit is on screen, expanding a fold if it is hiding it.
    fn reveal_current_match(&mut self) {
        let Some(row) = self
            .search
            .as_ref()
            .and_then(|s| s.current_match())
            .map(|m| m.row)
        else {
            return;
        };
        let Some(index) = model::view_index_of(&self.rows, row) else {
            return;
        };
        // A hit inside a collapsed run would otherwise be unreachable.
        if self.rows.get(index).is_some_and(|r| r.is_fold()) {
            self.cursor = index;
            self.expand_fold();
            if let Some(index) = model::view_index_of(&self.rows, row) {
                self.cursor = index;
            }
        } else {
            self.cursor = index;
        }
        self.top = self.cursor.saturating_sub(self.viewport as usize / 3);
    }

    /// Switch folding on or off, keeping the cursor on the same aligned row.
    pub fn toggle_fold(&mut self) {
        let anchor = self.cursor_row();
        self.context = match self.context {
            Some(_) => None,
            None => Some(DEFAULT_CONTEXT),
        };
        self.rebuild_rows();
        if let Some(anchor) = anchor
            && let Some(index) = model::view_index_of(&self.rows, anchor)
        {
            self.cursor = index;
            self.top = self.cursor.saturating_sub(self.viewport as usize / 3);
        }
    }

    /// Expand the fold under the cursor, without changing the global setting.
    pub fn expand_fold(&mut self) {
        let Some(ViewRow::Fold { start, end }) = self.rows.get(self.cursor).copied() else {
            return;
        };
        let replacement: Vec<ViewRow> = (start..end).map(ViewRow::Row).collect();
        self.rows.splice(self.cursor..=self.cursor, replacement);
    }

    /// Clamp scroll and cursor against the current viewport.
    ///
    /// Called at the top of every draw rather than from a resize handler, so it also
    /// covers the first frame and any freshly pushed view.
    pub fn clamp(&mut self, height: u16) {
        self.viewport = height.max(1);
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

    /// Jump to the next or previous hunk.
    ///
    /// Hunks are recorded against aligned rows, so the target is mapped back through the
    /// view — a hunk is never inside a fold, since folds only cover equal runs.
    fn goto_hunk(&mut self, forward: bool) {
        let Some(alignment) = self.cmp.alignment.as_ref() else {
            return;
        };
        let from = self.cursor_row().unwrap_or(0) as usize;
        let hunk = if forward {
            alignment.hunk_at_or_after(from + 1)
        } else {
            alignment.hunk_before(from)
        };
        if let Some(hunk) = hunk
            && let Some(index) = model::view_index_of(&self.rows, hunk.start)
        {
            self.cursor = index;
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

    pub fn clamp(&mut self, height: u16) {
        self.viewport = height.max(1);
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

/// A one-line text prompt at the bottom of the screen.
pub struct Prompt {
    pub label: &'static str,
    pub buffer: String,
}

pub struct App {
    pub views: Vec<View>,
    /// When set, keystrokes go here instead of to the active view.
    pub prompt: Option<Prompt>,
    /// A quit was asked for while changes were unsaved, and is waiting to be confirmed.
    pub quit_confirm: bool,
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
                (
                    vec![View::File(Box::new(FileView::with_context(
                        cmp,
                        opts.context,
                    )))],
                    code,
                )
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
            prompt: None,
            quit_confirm: false,
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
            self.request_quit();
            return;
        }
        // Leaving a file view with edits pending drops them just as surely as quitting.
        if matches!(self.views.last(), Some(View::File(f)) if f.is_dirty()) && !self.quit_confirm {
            self.quit_confirm = true;
            self.status =
                "unsaved changes \u{2014} press again to discard, or s to save".to_string();
            return;
        }
        self.views.pop();
        self.quit_confirm = false;
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
        let (max_size, diff_opts, context) =
            (self.opts.max_file_size, self.opts.diff, self.opts.context);
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
        let mut view = FileView::configured(cmp, context, diff_opts);
        view.hex_pinned = self.opts.hex_width_pinned;
        self.views.push(View::File(Box::new(view)));
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.ctrl;

        // The identical overlay is informational, not modal: the first keystroke clears it
        // *and* does what it normally would, so Esc still goes straight back.
        if let Some(view) = self.active_file() {
            view.notice = false;
        }

        // A pending quit confirmation only survives an immediate repeat: anything else
        // means the user carried on working, and a stale confirmation would then let a
        // later single keypress discard their edits.
        let repeat = matches!(
            key.code,
            KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::Esc
        );
        if !repeat {
            self.quit_confirm = false;
        }

        // While a prompt is open it owns the keyboard, so a query can contain q, j, or
        // anything else that would otherwise be a command.
        if self.prompt.is_some() {
            self.handle_prompt_key(key, ctrl);
            return;
        }

        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.request_quit();
                return;
            }
            KeyCode::Char('q') => {
                self.request_quit();
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

    /// Edit the open prompt. Enter runs it, Esc abandons it.
    fn handle_prompt_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            KeyCode::Esc => {
                self.prompt = None;
            }
            KeyCode::Enter => {
                let query = self.prompt.take().map(|p| p.buffer).unwrap_or_default();
                if query.is_empty() {
                    if let Some(view) = self.active_file() {
                        view.clear_search();
                    }
                    return;
                }
                if let Some(view) = self.active_file() {
                    view.run_search(&query);
                    let found = view.search.as_ref().map_or(0, |s| s.matches.len());
                    self.status = if found == 0 {
                        format!("no match for {query:?}")
                    } else {
                        String::new()
                    };
                }
            }
            KeyCode::Backspace => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.buffer.pop();
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.buffer.clear();
                }
            }
            KeyCode::Char('c') if ctrl => {
                self.prompt = None;
            }
            KeyCode::Char(c) => {
                if let Some(prompt) = self.prompt.as_mut() {
                    prompt.buffer.push(c);
                }
            }
            _ => {}
        }
    }

    fn handle_file_key(&mut self, key: KeyEvent, ctrl: bool) {
        if key.code == KeyCode::Char('/') {
            // Search reads line text, which a hex comparison does not have. Say so rather
            // than opening a prompt that could never match anything.
            if self.active_file().is_some_and(|v| v.cmp.hex.is_some()) {
                self.status = "search is not available in the hex view".to_string();
                return;
            }
            self.prompt = Some(Prompt {
                label: "/",
                buffer: String::new(),
            });
            self.status.clear();
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
            KeyCode::Char('H') => view.h_scroll = view.h_scroll.saturating_sub(8),
            KeyCode::Char('0') => view.h_scroll = 0,
            KeyCode::Char('z') => view.toggle_fold(),
            KeyCode::Char('>') => {
                let outcome = view.copy_hunk(SideIdx::LEFT);
                self.status = report(outcome);
            }
            KeyCode::Char('<') => {
                let outcome = view.copy_hunk(SideIdx::RIGHT);
                self.status = report(outcome);
            }
            // Plain `u` undoes; Ctrl-u still pages, matched by the arm above.
            KeyCode::Char('u') if !ctrl => {
                let outcome = view.undo();
                self.status = report(outcome);
            }
            KeyCode::Char('s') => {
                self.status = match view.save() {
                    Ok(message) | Err(message) => message,
                };
            }
            KeyCode::Char('N') => view.step_match(true),
            KeyCode::Char('P') => view.step_match(false),
            KeyCode::Enter => view.expand_fold(),
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

/// Turn an edit result into something for the status line.
fn report(result: Result<(), &'static str>) -> String {
    match result {
        Ok(()) => String::new(),
        Err(message) => message.to_string(),
    }
}

/// How long to wait for a key before looking at background work again.
const TICK: Duration = Duration::from_millis(100);

/// The event loop.
///
/// Redraws every iteration and blocks in `poll` otherwise, so an idle yadt costs nothing —
/// while a running directory scan still lands within one poll interval. blit writes only
/// the cells that changed, so an idle redraw sends nothing to the terminal.
///
/// The session restores the terminal when it is dropped, which covers an early `?` return
/// and a panic unwinding through here as well as the normal way out.
pub fn run_app(app: &mut App) -> Result<()> {
    let mut session = Session::new()?;
    release_mouse()?;
    let mut frame = Frame::default();
    let mut inputs = [Input::None; 32];

    while !app.should_quit {
        let info = session.frame_info();
        frame.render(session.platform_mut(), info, |builder: Ui<'_>| {
            ui::draw(builder, app);
        });
        session.present()?;
        app.tick();

        let poll = session.poll(Some(TICK), &mut inputs)?;
        if poll.resized {
            // Every cell on screen is stale, not just the ones the next frame changes.
            session.platform_mut().renderer_mut().invalidate();
        }
        for &input in &inputs[..poll.input_count] {
            if let Some(key) = KeyEvent::from_input(input) {
                app.handle_key(key);
            }
        }
    }
    session.finish()?;
    Ok(())
}

/// blit switches mouse reporting on when it takes the terminal. yadt switches it straight
/// back off: while it is on, the terminal's own text selection stops working, and selecting
/// a line of a diff to copy is a common thing to want.
fn release_mouse() -> std::io::Result<()> {
    use std::io::Write;

    let mut tty = std::fs::OpenOptions::new().write(true).open("/dev/tty")?;
    tty.write_all(b"\x1b[?1003l\x1b[?1006l")?;
    tty.flush()
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

#[cfg(test)]
mod fold_view_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::text::{FileContent, decode};

    fn content(body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new("x"),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    /// A file with a change, then a long identical run, then another change.
    fn view(context: Option<u32>) -> FileView {
        let left: String = std::iter::once("HEAD-LEFT")
            .chain(std::iter::repeat_n("same", 20))
            .chain(std::iter::once("TAIL-LEFT"))
            .collect::<Vec<_>>()
            .join("\n");
        let right: String = std::iter::once("HEAD-RIGHT")
            .chain(std::iter::repeat_n("same", 20))
            .chain(std::iter::once("TAIL-RIGHT"))
            .collect::<Vec<_>>()
            .join("\n");
        let cmp = compare_loaded(
            content(&format!("{left}\n")),
            content(&format!("{right}\n")),
            &DiffOptions::default(),
        );
        let mut v = FileView::with_context(cmp, context);
        v.viewport = 10;
        v
    }

    #[test]
    fn folding_shrinks_the_view_without_touching_the_alignment() {
        let unfolded = view(None);
        let folded = view(Some(3));

        let aligned = unfolded.cmp.alignment.as_ref().unwrap().len();
        assert_eq!(unfolded.row_count(), aligned);
        assert!(
            folded.row_count() < unfolded.row_count(),
            "folding should collapse the identical run"
        );
        // The alignment is identical in both -- folding is a view transform.
        assert_eq!(folded.cmp.alignment.as_ref().unwrap().len(), aligned);
        assert!(folded.hidden_rows() > 0);
        assert_eq!(unfolded.hidden_rows(), 0);
    }

    #[test]
    fn every_aligned_row_is_still_reachable_when_folded() {
        let folded = view(Some(3));
        let aligned = folded.cmp.alignment.as_ref().unwrap().len() as u32;
        let mut covered = Vec::new();
        for r in &folded.rows {
            match *r {
                ViewRow::Row(i) => covered.push(i),
                ViewRow::Fold { start, end } => covered.extend(start..end),
            }
        }
        assert_eq!(covered, (0..aligned).collect::<Vec<_>>());
    }

    #[test]
    fn toggling_keeps_the_cursor_on_the_same_aligned_row() {
        let mut v = view(None);
        // Park on the last row, which is a change.
        v.cursor = v.row_count() - 1;
        let before = v.cursor_row().unwrap();

        v.toggle_fold();
        assert_eq!(v.context, Some(DEFAULT_CONTEXT));
        assert_eq!(
            v.cursor_row(),
            Some(before),
            "the cursor drifted when folding"
        );

        v.toggle_fold();
        assert_eq!(v.context, None);
        assert_eq!(
            v.cursor_row(),
            Some(before),
            "the cursor drifted when unfolding"
        );
    }

    #[test]
    fn a_cursor_inside_a_hidden_run_lands_on_the_fold() {
        let mut v = view(None);
        // Row 10 is deep inside the identical run.
        v.cursor = 10;
        v.toggle_fold();
        assert!(
            v.cursor_is_fold(),
            "a cursor on a hidden row should land on the fold standing in for it"
        );
    }

    #[test]
    fn hunk_navigation_works_across_folds() {
        let mut v = view(Some(3));
        v.cursor = 0;
        let first = v.cursor_row().unwrap();

        v.goto_hunk(true);
        let second = v.cursor_row().unwrap();
        assert!(second > first, "n should move forward past the fold");
        assert!(!v.cursor_is_fold(), "a hunk is never inside a fold");

        v.goto_hunk(false);
        assert_eq!(v.cursor_row(), Some(first), "p should come back");
    }

    #[test]
    fn expanding_a_fold_reveals_exactly_its_rows() {
        let mut v = view(Some(3));
        let fold_index = v
            .rows
            .iter()
            .position(|r| r.is_fold())
            .expect("the run should be folded");
        let hidden = v.rows[fold_index].hidden();
        let before = v.row_count();

        v.cursor = fold_index;
        v.expand_fold();

        assert_eq!(v.row_count(), before - 1 + hidden as usize);
        assert!(v.rows.iter().all(|r| !r.is_fold()));
        assert_eq!(v.hidden_rows(), 0);
    }

    #[test]
    fn expanding_a_plain_row_does_nothing() {
        let mut v = view(Some(3));
        v.cursor = 0;
        let before = v.row_count();
        v.expand_fold();
        assert_eq!(v.row_count(), before);
    }

    #[test]
    fn folding_a_comparison_with_no_alignment_is_harmless() {
        let cmp = compare_loaded(
            content("text\n"),
            FileContent::Binary {
                path: "b".into(),
                size: 1,
                reason: crate::text::BinaryReason::Nul,
                bytes: vec![0],
            },
            &DiffOptions::default(),
        );
        let mut v = FileView::with_context(cmp, Some(3));
        assert_eq!(v.row_count(), 0);
        v.toggle_fold();
        v.expand_fold();
        v.goto_hunk(true);
        assert_eq!(v.cursor_row(), None);
    }
}

#[cfg(test)]
mod search_view_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::text::{FileContent, decode};

    fn content(body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new("x"),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    fn view(left: &str, right: &str) -> FileView {
        let cmp = compare_loaded(content(left), content(right), &DiffOptions::default());
        let mut v = FileView::with_context(cmp, None);
        v.viewport = 10;
        v
    }

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c)));
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::new(code));
    }

    fn app_with(view: FileView) -> App {
        App {
            views: vec![View::File(Box::new(view))],
            opts: Options {
                diff: DiffOptions::default(),
                tab_width: 8,
                max_file_size: u64::MAX,
                follow_symlinks: false,
                context: None,
                hex_width_pinned: false,
            },
            theme: Theme::no_color(),
            prompt: None,
            quit_confirm: false,
            help_visible: false,
            status: String::new(),
            should_quit: false,
            exit_code: 0,
        }
    }

    #[test]
    fn a_search_finds_hits_and_selects_the_first() {
        let mut v = view("alpha\nbeta\ngamma\n", "alpha\nbeta\ngamma\n");
        v.run_search("a");
        let state = v.search.as_ref().unwrap();
        assert!(!state.matches.is_empty());
        assert_eq!(state.current, 0);
        assert_eq!(state.query, "a");
    }

    #[test]
    fn stepping_wraps_around_the_ends() {
        let mut v = view("one\ntwo\n", "one\ntwo\n");
        v.run_search("o");
        let count = v.search.as_ref().unwrap().matches.len();
        assert!(count >= 2);

        for _ in 0..count {
            v.step_match(true);
        }
        assert_eq!(
            v.search.as_ref().unwrap().current,
            0,
            "stepping all the way round should come back to the first hit"
        );

        v.step_match(false);
        assert_eq!(v.search.as_ref().unwrap().current, count - 1);
    }

    #[test]
    fn a_search_moves_the_cursor_to_the_hit() {
        let mut v = view("aaa\nbbb\nneedle\n", "aaa\nbbb\nneedle\n");
        v.cursor = 0;
        v.run_search("needle");
        assert_eq!(v.cursor_row(), Some(2), "the cursor should follow the hit");
    }

    #[test]
    fn a_hit_hidden_by_a_fold_expands_it() {
        // A long identical run with the needle buried in the middle.
        let body: String = (0..30)
            .map(|i| {
                if i == 15 {
                    "needle".to_string()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let changed = format!("CHANGED\n{body}\nCHANGED\n");
        let other = format!("DIFFERENT\n{body}\nDIFFERENT\n");
        let cmp = compare_loaded(content(&changed), content(&other), &DiffOptions::default());
        let mut v = FileView::with_context(cmp, Some(2));
        v.viewport = 10;

        assert!(v.rows.iter().any(|r| r.is_fold()), "the run should fold");
        v.run_search("needle");

        assert!(
            !v.cursor_is_fold(),
            "the hit must be revealed, not left behind a fold"
        );
        // The revealed row is a real aligned row, not an index left dangling by the
        // expansion.
        assert!(
            v.cmp
                .alignment
                .as_ref()
                .unwrap()
                .row(v.cursor_row().unwrap() as usize)
                .is_some()
        );
    }

    #[test]
    fn a_search_with_no_hits_is_recorded_without_moving_anything() {
        let mut v = view("alpha\n", "alpha\n");
        v.cursor = 0;
        v.run_search("zzzz");
        let state = v.search.as_ref().unwrap();
        assert!(state.matches.is_empty());
        assert_eq!(v.cursor, 0);
        // Stepping a hitless search must not panic or divide by zero.
        v.step_match(true);
        v.step_match(false);
    }

    #[test]
    fn the_prompt_captures_keys_that_would_otherwise_be_commands() {
        let mut app = app_with(view("query here\n", "query here\n"));
        press(&mut app, '/');
        assert!(app.prompt.is_some());

        // 'q' would quit outside the prompt; here it is just a letter.
        for c in "query".chars() {
            press(&mut app, c);
        }
        assert!(!app.should_quit, "typing q in a prompt must not quit");
        assert_eq!(app.prompt.as_ref().unwrap().buffer, "query");

        key(&mut app, KeyCode::Backspace);
        assert_eq!(app.prompt.as_ref().unwrap().buffer, "quer");

        key(&mut app, KeyCode::Enter);
        assert!(app.prompt.is_none(), "Enter should close the prompt");
        assert!(app.active_file().unwrap().search.is_some());
    }

    #[test]
    fn escape_abandons_the_prompt_without_searching() {
        let mut app = app_with(view("alpha\n", "alpha\n"));
        press(&mut app, '/');
        press(&mut app, 'a');
        key(&mut app, KeyCode::Esc);
        assert!(app.prompt.is_none());
        assert!(app.active_file().unwrap().search.is_none());
        assert!(!app.should_quit, "Esc in a prompt must not pop the view");
    }

    #[test]
    fn an_empty_query_clears_the_previous_search() {
        let mut app = app_with(view("alpha\n", "alpha\n"));
        press(&mut app, '/');
        press(&mut app, 'a');
        key(&mut app, KeyCode::Enter);
        assert!(app.active_file().unwrap().search.is_some());

        press(&mut app, '/');
        key(&mut app, KeyCode::Enter);
        assert!(app.active_file().unwrap().search.is_none());
    }

    #[test]
    fn a_failed_search_says_so() {
        let mut app = app_with(view("alpha\n", "alpha\n"));
        press(&mut app, '/');
        for c in "zzz".chars() {
            press(&mut app, c);
        }
        key(&mut app, KeyCode::Enter);
        assert!(
            app.status.contains("no match"),
            "status was {:?}",
            app.status
        );
    }

    #[test]
    fn hunk_keys_still_work_while_a_search_is_active() {
        // n/p stay bound to hunks; matches are on N/P. No modes.
        let mut app = app_with(view("same\nold\n", "same\nnew\n"));
        press(&mut app, '/');
        press(&mut app, 's');
        key(&mut app, KeyCode::Enter);

        press(&mut app, 'n');
        let view = app.active_file().unwrap();
        assert_eq!(
            view.cursor_row(),
            Some(1),
            "n should still jump to the hunk"
        );
    }
}

#[cfg(test)]
mod hex_view_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::text::{BinaryReason, FileContent};

    fn binary(bytes: &[u8]) -> FileContent {
        FileContent::Binary {
            path: "b".into(),
            size: bytes.len() as u64,
            reason: BinaryReason::Nul,
            bytes: bytes.to_vec(),
        }
    }

    fn hex_view(left: &[u8], right: &[u8]) -> FileView {
        let opts = DiffOptions {
            hex_bytes_per_row: 4,
            ..DiffOptions::default()
        };
        let cmp = compare_loaded(binary(left), binary(right), &opts);
        let mut v = FileView::with_context(cmp, None);
        v.viewport = 10;
        v
    }

    fn app_with(view: FileView) -> App {
        App {
            views: vec![View::File(Box::new(view))],
            opts: Options {
                diff: DiffOptions::default(),
                tab_width: 8,
                max_file_size: u64::MAX,
                follow_symlinks: false,
                context: None,
                hex_width_pinned: false,
            },
            theme: Theme::no_color(),
            prompt: None,
            quit_confirm: false,
            help_visible: false,
            status: String::new(),
            should_quit: false,
            exit_code: 0,
        }
    }

    #[test]
    fn two_binaries_get_a_hex_comparison_rather_than_a_bare_verdict() {
        let v = hex_view(b"abcdefgh", b"abcdXfgh");
        assert_eq!(v.cmp.hex, Some(4));
        assert!(v.cmp.alignment.is_some(), "hex still produces an alignment");
        assert!(v.cmp.differ);
        assert!(!v.cmp.failed);
        assert_eq!(v.row_count(), 2);
    }

    #[test]
    fn identical_binaries_report_no_difference() {
        let v = hex_view(b"same bytes", b"same bytes");
        assert!(!v.cmp.differ);
        assert!(v.cmp.alignment.as_ref().unwrap().is_identical());
    }

    #[test]
    fn hunk_navigation_works_over_bytes() {
        // Differ in the first and last chunk, match in between.
        let v_left = b"XXXX....++++";
        let v_right = b"YYYY....----";
        let mut v = hex_view(v_left, v_right);
        assert_eq!(v.cmp.alignment.as_ref().unwrap().hunks().len(), 2);

        v.cursor = 0;
        v.goto_hunk(true);
        assert_eq!(v.cursor_row(), Some(2), "n should reach the second hunk");
    }

    #[test]
    fn a_text_file_against_a_binary_is_still_not_comparable() {
        let text = FileContent::Text(Box::new(crate::text::decode(
            std::path::Path::new("t"),
            b"hello\n".to_vec(),
            None,
        )));
        let cmp = compare_loaded(text, binary(b"\x00\x01"), &DiffOptions::default());
        assert_eq!(cmp.hex, None, "a hex view needs bytes on both sides");
        assert!(cmp.alignment.is_none());
    }

    #[test]
    fn search_is_refused_in_the_hex_view_rather_than_silently_failing() {
        let mut app = app_with(hex_view(b"abcd", b"abcX"));
        app.handle_key(KeyEvent::new(KeyCode::Char('/')));
        assert!(app.prompt.is_none(), "no prompt should open");
        assert!(
            app.status.contains("not available"),
            "status was {:?}",
            app.status
        );
    }

    #[test]
    fn folding_still_works_in_the_hex_view() {
        let mut left = vec![0u8; 200];
        left[0] = 1;
        let mut right = left.clone();
        right[0] = 2;
        let mut v = hex_view(&left, &right);
        let unfolded = v.row_count();

        v.toggle_fold();
        assert!(
            v.row_count() < unfolded,
            "the identical run should collapse"
        );
        assert!(v.hidden_rows() > 0);
    }
}

impl FileView {
    /// Re-fit the hex row width to the space available, rebuilding the alignment if the
    /// width actually changes.
    ///
    /// Bytes per row define the rows themselves, so this cannot be a pure render-time
    /// decision the way dropping the ASCII pane is. It is gated on the width *bucket*
    /// changing, so dragging a terminal edge within one bucket costs nothing, and a
    /// pinned `--hex-width` is left alone.
    pub fn fit_hex_width(&mut self, pane_text_width: u16) {
        let Some(current) = self.cmp.hex else {
            return;
        };
        if self.hex_pinned {
            return;
        }
        let desired = crate::hex::fit_bytes_per_row(pane_text_width);
        if desired == current {
            return;
        }
        // Re-aligning a large binary takes long enough to be felt as a stall, and the
        // startup fit already used the real terminal size.
        let size = self
            .cmp
            .left
            .as_binary()
            .map_or(0, <[u8]>::len)
            .max(self.cmp.right.as_binary().map_or(0, <[u8]>::len));
        if size > crate::hex::REFIT_SIZE_LIMIT {
            return;
        }

        // Hold the byte offset, not the row index: the same row number means a different
        // place in the file once the width changes.
        let offset = self
            .cursor_row()
            .map(|row| crate::hex::row_offset(row, current));

        let alignment = {
            let (Some(left), Some(right)) = (self.cmp.left.as_binary(), self.cmp.right.as_binary())
            else {
                return;
            };
            crate::hex::align_bytes(left, right, desired)
        };
        self.cmp.alignment = Some(alignment);
        self.cmp.hex = Some(desired);
        self.rebuild_rows();

        if let Some(offset) = offset {
            let row = (offset / u64::from(desired)) as u32;
            if let Some(index) = model::view_index_of(&self.rows, row) {
                self.cursor = index;
                self.top = self.cursor.saturating_sub(self.viewport as usize / 3);
            }
        }
        // A search is over line text, which a hex view has none of, but be tidy anyway.
        self.search = None;
    }
}

#[cfg(test)]
mod hex_fit_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::text::{BinaryReason, FileContent};

    fn binary(bytes: &[u8]) -> FileContent {
        FileContent::Binary {
            path: "b".into(),
            size: bytes.len() as u64,
            reason: BinaryReason::Nul,
            bytes: bytes.to_vec(),
        }
    }

    fn view(len: usize, per_row: u32) -> FileView {
        let mut left = vec![0u8; len];
        left[0] = 1;
        let mut right = left.clone();
        right[len - 1] = 9;
        let opts = DiffOptions {
            hex_bytes_per_row: per_row,
            ..DiffOptions::default()
        };
        let cmp = compare_loaded(binary(&left), binary(&right), &opts);
        let mut v = FileView::with_context(cmp, None);
        v.viewport = 20;
        v
    }

    #[test]
    fn a_wider_pane_packs_more_bytes_into_each_row() {
        let mut v = view(256, 8);
        assert_eq!(v.cmp.hex, Some(8));
        let rows_at_8 = v.row_count();

        v.fit_hex_width(100); // room for 24
        assert_eq!(v.cmp.hex, Some(24));
        assert!(v.row_count() < rows_at_8, "wider rows mean fewer of them");
    }

    #[test]
    fn a_narrower_pane_goes_back_down() {
        let mut v = view(256, 32);
        v.fit_hex_width(40);
        assert_eq!(v.cmp.hex, Some(8));
    }

    #[test]
    fn refitting_holds_the_byte_offset_not_the_row_number() {
        let mut v = view(256, 8);
        // Row 10 at 8 bytes per row is byte 80.
        v.cursor = 10;
        assert_eq!(v.cursor_row(), Some(10));

        v.fit_hex_width(100); // 24 bytes per row
        let row = v.cursor_row().unwrap();
        let offset = crate::hex::row_offset(row, 24);
        assert!(
            (offset..offset + 24).contains(&80),
            "byte 80 should still be on screen; landed at offset {offset}"
        );
    }

    #[test]
    fn an_unchanged_width_rebuilds_nothing() {
        let mut v = view(256, 8);
        let before = v.row_count();
        v.cursor = 5;
        v.fit_hex_width(35); // still 8
        assert_eq!(v.cmp.hex, Some(8));
        assert_eq!(v.row_count(), before);
        assert_eq!(v.cursor, 5, "an unchanged fit must not disturb the cursor");
    }

    #[test]
    fn a_pinned_width_is_left_alone() {
        let mut v = view(256, 8);
        v.hex_pinned = true;
        v.fit_hex_width(300);
        assert_eq!(v.cmp.hex, Some(8), "--hex-width must win over the terminal");
    }

    #[test]
    fn a_large_binary_is_not_refitted_live() {
        // Re-aligning this would stall; the startup fit already used the real terminal.
        let mut v = view(crate::hex::REFIT_SIZE_LIMIT + 1024, 8);
        v.fit_hex_width(300);
        assert_eq!(v.cmp.hex, Some(8));
    }

    #[test]
    fn a_text_comparison_ignores_the_fit_entirely() {
        let text = |b: &str| {
            FileContent::Text(Box::new(crate::text::decode(
                std::path::Path::new("t"),
                b.as_bytes().to_vec(),
                None,
            )))
        };
        let cmp = compare_loaded(text("a\n"), text("b\n"), &DiffOptions::default());
        let mut v = FileView::with_context(cmp, None);
        let before = v.row_count();
        v.fit_hex_width(300);
        assert_eq!(v.cmp.hex, None);
        assert_eq!(v.row_count(), before);
    }

    #[test]
    fn the_alignment_stays_correct_after_a_refit() {
        // The last byte differs; it must still be the one marked, at the new width.
        let mut v = view(256, 8);
        v.fit_hex_width(100);
        let alignment = v.cmp.alignment.as_ref().unwrap();
        assert!(!alignment.is_identical());

        let last = alignment.len() - 1;
        let spans: Vec<_> = alignment
            .emphasis(last, SideIdx::LEFT)
            .map(|e| (e.start, e.end))
            .collect();
        assert_eq!(spans, [(255 % 24, 255 % 24 + 1)]);
    }
}

// ---------------------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------------------

impl FileView {
    /// Can this comparison be edited at all?
    ///
    /// Only a text-versus-text diff: a hex view has no lines to copy, and anything that
    /// failed to load has no content to copy from.
    pub fn editable(&self) -> bool {
        self.cmp.hex.is_none() && self.cmp.alignment.is_some() && self.cmp.texts().is_some()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty.iter().any(|d| *d)
    }

    fn side_text(&self, side: SideIdx) -> Option<&crate::text::TextFile> {
        let content = if side == SideIdx::LEFT {
            &self.cmp.left
        } else {
            &self.cmp.right
        };
        content.as_text()
    }

    fn set_side_text(&mut self, side: SideIdx, file: crate::text::TextFile) {
        let slot = if side == SideIdx::LEFT {
            &mut self.cmp.left
        } else {
            &mut self.cmp.right
        };
        *slot = crate::text::FileContent::Text(Box::new(file));
    }

    /// Recompute the diff after one side changed, keeping the cursor near `anchor`.
    fn rediff(&mut self, anchor: u32) {
        let Some((left, right)) = self.cmp.texts() else {
            return;
        };
        let alignment = crate::diff::align_text(left, right, &self.diff_opts);
        self.cmp.differ = !alignment.is_identical();
        self.cmp.alignment = Some(alignment);
        self.rebuild_rows();
        self.search = None;

        if let Some(index) = model::view_index_of(&self.rows, anchor) {
            self.cursor = index;
            self.top = self.cursor.saturating_sub(self.viewport as usize / 3);
        }
    }

    /// Copy the hunk under the cursor from `from` onto the other side.
    ///
    /// Returns what happened, for the status line.
    pub fn copy_hunk(&mut self, from: SideIdx) -> Result<(), &'static str> {
        if !self.editable() {
            return Err("this comparison cannot be edited");
        }
        let cursor_row = self.cursor_row().unwrap_or(0);
        let hunk = self
            .cmp
            .alignment
            .as_ref()
            .and_then(|a| a.hunk_containing(cursor_row as usize))
            .ok_or("the cursor is not on a change")?;

        let to = if from == SideIdx::LEFT {
            SideIdx::RIGHT
        } else {
            SideIdx::LEFT
        };

        let applied = {
            let alignment = self.cmp.alignment.as_ref().ok_or("no diff")?;
            let source = self.side_text(from).ok_or("no diff")?;
            let target = self.side_text(to).ok_or("no diff")?;
            edit::copy_hunk(alignment, hunk, from, source, to, target, cursor_row)
        };
        let Some(applied) = applied else {
            return Err("the two sides already agree here");
        };

        self.set_side_text(to, applied.file);
        self.undo_log.push(applied.edit);
        // Oldest first, so dropping from the front keeps the most recent history.
        if self.undo_log.len() > edit::UNDO_LIMIT {
            self.undo_log.remove(0);
        }
        self.dirty[to.idx()] = true;
        self.rediff(cursor_row);
        Ok(())
    }

    /// Reverse the most recent edit.
    pub fn undo(&mut self) -> Result<(), &'static str> {
        let Some(edit) = self.undo_log.pop() else {
            return Err("nothing to undo");
        };
        let restored = {
            let current = self.side_text(edit.side).ok_or("no diff")?;
            edit::undo(&edit, current)
        };
        self.set_side_text(edit.side, restored);
        // The side stays dirty unless this was the only edit to it, in which case it is
        // back to what was read from disk.
        self.dirty[edit.side.idx()] = self.undo_log.iter().any(|e| e.side == edit.side);
        self.rediff(edit.cursor_row);
        Ok(())
    }

    /// Write every changed side back to disk.
    pub fn save(&mut self) -> Result<String, String> {
        if !self.is_dirty() {
            return Err("nothing to save".to_string());
        }
        let mut saved = Vec::new();
        for side in [SideIdx::LEFT, SideIdx::RIGHT] {
            if !self.dirty[side.idx()] {
                continue;
            }
            let Some(file) = self.side_text(side) else {
                continue;
            };
            let (path, bytes) = (file.path.clone(), file.to_bytes());
            let expected = self.cmp.stamps[side.idx()].unwrap_or_default();

            match crate::text::save_atomic(&path, &bytes, expected) {
                Ok(stamp) => {
                    self.cmp.stamps[side.idx()] = Some(stamp);
                    self.dirty[side.idx()] = false;
                    saved.push(path.file_name().map_or_else(
                        || path.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    ));
                }
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(format!("saved {}", saved.join(", ")))
    }
}

/// Editing tests. These use the real filesystem for the save path on purpose: an atomic
/// rename and a staleness check cannot be exercised against an in-memory fake.
#[cfg(test)]
mod edit_view_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_files};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("yadt-edit-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, body: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn view(left: &Path, right: &Path) -> FileView {
        let cmp = compare_files(left, right, u64::MAX, &DiffOptions::default());
        let mut v = FileView::with_context(cmp, None);
        v.viewport = 20;
        v
    }

    fn side_text(v: &FileView, side: SideIdx) -> String {
        let content = if side == SideIdx::LEFT {
            &v.cmp.left
        } else {
            &v.cmp.right
        };
        String::from_utf8(content.as_text().unwrap().to_bytes()).unwrap()
    }

    /// Put the cursor on the first change.
    fn on_first_hunk(v: &mut FileView) {
        v.cursor = 0;
        v.goto_hunk(true);
        if v.cmp
            .alignment
            .as_ref()
            .unwrap()
            .hunk_containing(0)
            .is_some()
        {
            v.cursor = 0;
        }
    }

    #[test]
    fn copying_left_to_right_changes_only_the_right() {
        let s = Scratch::new("l2r");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);

        assert!(v.copy_hunk(SideIdx::LEFT).is_ok());
        assert_eq!(side_text(&v, SideIdx::RIGHT), "a\nLEFT\nb\n");
        assert_eq!(side_text(&v, SideIdx::LEFT), "a\nLEFT\nb\n");
        assert_eq!(v.dirty, [false, true], "only the target is dirty");
        // The sides now agree, so the diff reflects it.
        assert!(v.cmp.alignment.as_ref().unwrap().is_identical());
        assert!(!v.cmp.differ);
    }

    #[test]
    fn copying_right_to_left_changes_only_the_left() {
        let s = Scratch::new("r2l");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);

        assert!(v.copy_hunk(SideIdx::RIGHT).is_ok());
        assert_eq!(side_text(&v, SideIdx::LEFT), "a\nright\nb\n");
        assert_eq!(v.dirty, [true, false]);
    }

    #[test]
    fn undo_puts_it_back_and_clears_the_dirty_flag() {
        let s = Scratch::new("undo");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);

        v.copy_hunk(SideIdx::LEFT).unwrap();
        assert!(v.is_dirty());

        v.undo().unwrap();
        assert_eq!(side_text(&v, SideIdx::RIGHT), "a\nright\nb\n");
        assert!(!v.is_dirty(), "back to what was read, so nothing to save");
        assert!(v.undo_log.is_empty());
        assert!(v.cmp.differ, "the difference is back");
    }

    #[test]
    fn undo_unwinds_several_edits_in_order() {
        let s = Scratch::new("multi");
        let (l, r) = (
            s.file("l", "A\nb\nC\nd\nE\n"),
            s.file("r", "1\nb\n3\nd\n5\n"),
        );
        let mut v = view(&l, &r);

        // Copy every hunk across, one at a time.
        let mut copied = 0;
        while let Some(hunk) = v
            .cmp
            .alignment
            .as_ref()
            .and_then(|a| a.hunks().first().copied())
        {
            v.cursor = model::view_index_of(&v.rows, hunk.start).unwrap();
            if v.copy_hunk(SideIdx::LEFT).is_err() {
                break;
            }
            copied += 1;
        }
        assert!(copied >= 3, "expected several hunks, copied {copied}");
        assert_eq!(side_text(&v, SideIdx::RIGHT), "A\nb\nC\nd\nE\n");

        for _ in 0..copied {
            v.undo().unwrap();
        }
        assert_eq!(side_text(&v, SideIdx::RIGHT), "1\nb\n3\nd\n5\n");
        assert!(!v.is_dirty());
    }

    #[test]
    fn undoing_with_no_history_says_so() {
        let s = Scratch::new("noundo");
        let (l, r) = (s.file("l", "a\n"), s.file("r", "b\n"));
        let mut v = view(&l, &r);
        assert_eq!(v.undo(), Err("nothing to undo"));
    }

    #[test]
    fn copying_off_a_change_is_refused() {
        let s = Scratch::new("nohunk");
        let (l, r) = (s.file("l", "same\nx\n"), s.file("r", "same\ny\n"));
        let mut v = view(&l, &r);
        v.cursor = 0; // row 0 is equal
        assert_eq!(
            v.copy_hunk(SideIdx::LEFT),
            Err("the cursor is not on a change")
        );
        assert!(!v.is_dirty(), "a refused copy must not mark anything dirty");
    }

    #[test]
    fn saving_writes_the_file_and_clears_the_flag() {
        let s = Scratch::new("save");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);
        v.copy_hunk(SideIdx::LEFT).unwrap();

        let message = v.save().expect("the save should succeed");
        assert!(message.contains("saved"), "{message}");
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "a\nLEFT\nb\n");
        // The untouched side is left exactly as it was.
        assert_eq!(std::fs::read_to_string(&l).unwrap(), "a\nLEFT\nb\n");
        assert!(!v.is_dirty());
    }

    #[test]
    fn saving_with_nothing_to_save_is_refused() {
        let s = Scratch::new("nosave");
        let (l, r) = (s.file("l", "a\n"), s.file("r", "b\n"));
        let mut v = view(&l, &r);
        assert!(v.save().is_err());
        // And nothing was touched.
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "b\n");
    }

    #[test]
    fn saving_refuses_a_file_that_changed_underneath() {
        let s = Scratch::new("stale");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);
        v.copy_hunk(SideIdx::LEFT).unwrap();

        // Somebody else edits it while we were looking.
        std::fs::write(&r, "someone else was here\n").unwrap();

        let err = v
            .save()
            .expect_err("a changed file must not be overwritten");
        assert!(err.contains("changed on disk"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&r).unwrap(),
            "someone else was here\n",
            "their work must survive"
        );
        assert!(v.is_dirty(), "our edit is still pending");
    }

    #[test]
    fn saving_leaves_no_temporary_behind() {
        let s = Scratch::new("tmp");
        let (l, r) = (s.file("l", "a\nX\n"), s.file("r", "a\ny\n"));
        let mut v = view(&l, &r);
        on_first_hunk(&mut v);
        v.copy_hunk(SideIdx::LEFT).unwrap();
        v.save().unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(&s.0)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("yadt-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_hex_comparison_cannot_be_edited() {
        let s = Scratch::new("hex");
        let (l, r) = (s.file("l", "a\0b"), s.file("r", "a\0c"));
        let mut v = view(&l, &r);
        assert!(!v.editable());
        assert_eq!(
            v.copy_hunk(SideIdx::LEFT),
            Err("this comparison cannot be edited")
        );
    }

    #[test]
    fn the_keys_are_wired_up() {
        let s = Scratch::new("keys");
        let (l, r) = (s.file("l", "a\nLEFT\nb\n"), s.file("r", "a\nright\nb\n"));
        let cmp = compare_files(&l, &r, u64::MAX, &DiffOptions::default());
        let mut app = App {
            views: vec![View::File(Box::new(FileView::with_context(cmp, None)))],
            opts: Options {
                diff: DiffOptions::default(),
                tab_width: 8,
                max_file_size: u64::MAX,
                follow_symlinks: false,
                context: None,
                hex_width_pinned: false,
            },
            theme: Theme::no_color(),
            prompt: None,
            quit_confirm: false,
            help_visible: false,
            status: String::new(),
            should_quit: false,
            exit_code: 0,
        };
        let press = |app: &mut App, c: char| {
            app.handle_key(KeyEvent::new(KeyCode::Char(c)));
        };

        app.active_file().unwrap().cursor = 1;
        press(&mut app, '>');
        assert_eq!(
            side_text(app.active_file().unwrap(), SideIdx::RIGHT),
            "a\nLEFT\nb\n"
        );

        press(&mut app, 'u');
        assert_eq!(
            side_text(app.active_file().unwrap(), SideIdx::RIGHT),
            "a\nright\nb\n"
        );

        // 'u' alone undoes; Ctrl-u must still page.
        app.active_file().unwrap().cursor = 1;
        press(&mut app, '>');
        app.handle_key(KeyEvent::ctrl(KeyCode::Char('u')));
        assert!(
            app.active_file().unwrap().is_dirty(),
            "Ctrl-u should page, not undo"
        );

        press(&mut app, 's');
        assert!(app.status.contains("saved"), "status was {:?}", app.status);
        assert_eq!(std::fs::read_to_string(&r).unwrap(), "a\nLEFT\nb\n");
    }
}

impl App {
    /// Are there edits on any view that have not reached disk?
    pub fn has_unsaved(&self) -> bool {
        self.views.iter().any(|v| match v {
            View::File(f) => f.is_dirty(),
            View::Dir(_) => false,
        })
    }

    /// Quit, unless that would throw work away without saying so.
    ///
    /// The second press goes through. Leaving without warning would be the one way this
    /// tool could lose something the user typed.
    fn request_quit(&mut self) {
        if self.has_unsaved() && !self.quit_confirm {
            self.quit_confirm = true;
            self.status =
                "unsaved changes \u{2014} press again to discard, or s to save".to_string();
            return;
        }
        self.should_quit = true;
    }
}

#[cfg(test)]
mod quit_guard_tests {
    use super::*;
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::text::{FileContent, decode};

    fn text(body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new("f"),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    fn app() -> App {
        let cmp = compare_loaded(text("a\nX\n"), text("a\ny\n"), &DiffOptions::default());
        App {
            views: vec![View::File(Box::new(FileView::with_context(cmp, None)))],
            opts: Options {
                diff: DiffOptions::default(),
                tab_width: 8,
                max_file_size: u64::MAX,
                follow_symlinks: false,
                context: None,
                hex_width_pinned: false,
            },
            theme: Theme::no_color(),
            prompt: None,
            quit_confirm: false,
            help_visible: false,
            status: String::new(),
            should_quit: false,
            exit_code: 0,
        }
    }

    fn press(app: &mut App, c: char) {
        app.handle_key(KeyEvent::new(KeyCode::Char(c)));
    }

    fn dirty_app() -> App {
        let mut app = app();
        let view = app.active_file().unwrap();
        view.cursor = 1;
        view.copy_hunk(SideIdx::LEFT).unwrap();
        assert!(app.has_unsaved());
        app
    }

    #[test]
    fn quitting_with_no_edits_needs_no_confirmation() {
        let mut app = app();
        press(&mut app, 'q');
        assert!(app.should_quit);
    }

    #[test]
    fn quitting_with_edits_asks_first() {
        let mut app = dirty_app();
        press(&mut app, 'q');
        assert!(!app.should_quit, "the first press must not discard edits");
        assert!(
            app.status.contains("unsaved"),
            "status was {:?}",
            app.status
        );

        press(&mut app, 'q');
        assert!(app.should_quit, "the second press goes through");
    }

    #[test]
    fn escape_out_of_a_dirty_view_asks_too() {
        let mut app = dirty_app();
        app.handle_key(KeyEvent::new(KeyCode::Esc));
        assert!(!app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::Esc));
        assert!(app.should_quit);
    }

    #[test]
    fn carrying_on_working_cancels_the_pending_confirmation() {
        // Otherwise a q pressed minutes ago would let a single later q throw work away.
        let mut app = dirty_app();
        press(&mut app, 'q');
        assert!(app.quit_confirm);

        press(&mut app, 'j');
        assert!(!app.quit_confirm, "moving the cursor cancels it");

        press(&mut app, 'q');
        assert!(!app.should_quit, "so this is a first press again");
    }

    #[test]
    fn saving_removes_the_need_to_confirm() {
        let mut app = dirty_app();
        // Pretend the save succeeded; the flag is what the guard reads.
        app.active_file().unwrap().dirty = [false; 2];
        press(&mut app, 'q');
        assert!(app.should_quit);
    }

    #[test]
    fn undoing_back_to_clean_removes_it_too() {
        let mut app = dirty_app();
        app.active_file().unwrap().undo().unwrap();
        assert!(!app.has_unsaved());
        press(&mut app, 'q');
        assert!(app.should_quit);
    }
}
