//! Layout and rendering: free functions that describe one blit frame.
//!
//! blit lays a frame out after the tree has been built, but yadt needs the body's height
//! *while* building, to clamp the scroll position before choosing which rows to draw. So the
//! screen is divided here with plain arithmetic, the views are clamped against those numbers,
//! and each region is placed at exactly that rectangle with `layout::rect`. What the clamp
//! assumed and where the rows land cannot disagree, because they are the same numbers.
//!
//! Anything that carries a background across a pane is painted cell by cell through a
//! [`Canvas`]. One-line chrome, panels and the help overlay are blit's `Text` and `Block`.

use std::time::Duration;

use blit::{Frame, FrameInfo, Input, state};
use blit_tui::atom::Border;
use blit_tui::color::Color as TermColor;
use blit_tui::layout::rect;
use blit_tui::text::{Span, TextOptions, TextWrap};
use blit_tui::widget::{Block, Text, Title};
use blit_tui::{TuiContext, Ui};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DirView, FileView, View};
use crate::canvas::{self, Canvas};
use crate::diff::human_size;
use crate::dirdiff::{EntryKind, EntryStatus};
use crate::hex;
use crate::model::{Cell, RowKind, SideIdx};
use crate::pane::DiffPane;
use crate::style::Style;
use crate::text::{FileContent, TextFile};
use crate::theme::Theme;

/// Width of the column between the two panes.
const MARKER_WIDTH: u16 = 3;

/// The frame's root node, which puts each child at an exact rectangle.
type Root<'a> = Ui<'a, state::Open<rect::Layout>>;

/// A region of the screen, in whole cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Rect {
    const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// What is left inside a one-cell border.
    const fn inner(self) -> Self {
        Self {
            x: self.x.saturating_add(1),
            y: self.y.saturating_add(1),
            width: self.width.saturating_sub(2),
            height: self.height.saturating_sub(2),
        }
    }
}

impl From<Rect> for blit::PhysicalRect {
    fn from(area: Rect) -> Self {
        Self::new(
            i32::from(area.x),
            i32::from(area.y),
            i32::from(area.width),
            i32::from(area.height),
        )
    }
}

/// One frame through blit's three passes: build the tree, lay it out, paint it into the
/// renderer, whose output then holds what the terminal needs to be sent.
pub fn render_frame(
    frame: &mut Frame<TuiContext>,
    context: &mut TuiContext,
    now: Duration,
    build: impl FnOnce(Ui<'_>),
) {
    let info = FrameInfo::new(context.renderer().screen().size());
    frame.build(context, info, now, Input::None, build);
    frame.layout(context);
    context.begin_paint();
    frame.paint(context);
    context.finish_paint();
}

/// Takes `&mut App` because scroll clamping happens here — doing it anywhere else leaves
/// one frame of garbage after a resize or a view push.
pub fn draw(ui: Ui<'_>, app: &mut App) {
    let screen = ui.screen();
    let area = Rect::new(0, 0, cells(screen.width), cells(screen.height));
    let [header, body, status] = screen_rows(area);
    let theme = app.theme;
    let tab_width = app.opts.tab_width;
    let message = app.status.clone();
    let mut root = ui.layout(rect::new());

    match app.views.last_mut() {
        Some(View::File(view)) => {
            view.clamp(body.height);
            if view.cmp.hex.is_some() {
                // The gutter is sized from the file, so what is left for the row is
                // whatever the pane has after it.
                let pane = pane_columns(body)[0];
                let gutter = hex_gutter_width(view);
                view.fit_hex_width(pane.width.saturating_sub(gutter + 1));
            }
            draw_file_header(&mut root, header, view, &theme);
            draw_file_body(&mut root, body, view, &theme, tab_width);
            draw_file_status(&mut root, status, view, &theme, &message);
        }
        Some(View::Dir(view)) => {
            view.clamp(body.height);
            draw_dir_header(&mut root, header, view, &theme);
            draw_dir_body(&mut root, body, view, &theme);
            draw_dir_status(&mut root, status, view, &theme, &message);
        }
        None => {}
    }

    if let Some(prompt) = app.prompt.as_ref() {
        line(
            &mut root,
            status,
            &[
                (prompt.label, theme.header_focus),
                (prompt.buffer.as_str(), theme.status),
                ("\u{2588}", theme.header_focus),
            ],
        );
    }

    if let Some(View::File(view)) = app.views.last()
        && view.notice
    {
        draw_identical_notice(&mut root, body, view, &theme);
    }

    if app.help_visible {
        draw_help(
            &mut root,
            area,
            &theme,
            matches!(app.views.last(), Some(View::Dir(_))),
        );
    }
}

/// A frame extent in whole cells, in the `u16` the rest of the layout works in.
fn cells(extent: i32) -> u16 {
    u16::try_from(extent.max(0)).unwrap_or(u16::MAX)
}

// ---------------------------------------------------------------------------------------
// Placing things on screen
// ---------------------------------------------------------------------------------------

/// Put a canvas on screen at `area`.
fn place(root: &mut Root<'_>, area: Rect, canvas: Canvas) {
    root.child().item(area.into()).insert(canvas);
}

/// One line of styled parts along the top of `area`, clipped to its width.
fn line(root: &mut Root<'_>, area: Rect, parts: &[(&str, Style)]) {
    let spans: Vec<Span<'_>> = parts
        .iter()
        .map(|&(text, style)| canvas::span(text, style))
        .collect();
    root.child().item(area.into()).insert(Text::rich(&spans));
}

/// Lines of styled parts, filling `area` from the top.
fn paragraph(root: &mut Root<'_>, area: Rect, lines: &[Vec<(String, Style)>], wrap: TextWrap) {
    let mut spans = Vec::new();
    for (i, parts) in lines.iter().enumerate() {
        if i > 0 {
            spans.push(Span::new("\n"));
        }
        spans.extend(parts.iter().map(|(text, style)| canvas::span(text, *style)));
    }
    root.child()
        .item(area.into())
        .insert(Text::rich(&spans).options(TextOptions::new().wrap(wrap)));
}

/// A bordered panel, cleared first so nothing beneath shows through it.
fn panel(root: &mut Root<'_>, area: Rect, title: Option<&str>) {
    let mut block = Block::new()
        .background(TermColor::Reset)
        .border(Border::new(TermColor::Reset));
    if let Some(title) = title {
        block = block.title(Title::new(title));
    }
    root.child().item(area.into()).insert(block);
}

// ---------------------------------------------------------------------------------------
// Directory view
// ---------------------------------------------------------------------------------------

fn draw_dir_header(root: &mut Root<'_>, area: Rect, view: &DirView, theme: &Theme) {
    let [left, _, right] = pane_columns(area);
    render_path(root, left, &view.roots[0], false, theme);
    render_path(root, right, &view.roots[1], false, theme);
}

fn draw_dir_body(root: &mut Root<'_>, area: Rect, view: &mut DirView, theme: &Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (top, cursor, filter) = (view.top, view.cursor, view.filter);
    let visible: Vec<usize> = view.visible().to_vec();

    if visible.is_empty() {
        let text = if view.done {
            match filter {
                crate::app::DirFilter::All => "the two directories are empty".to_string(),
                _ => format!("nothing to show with filter '{}' — press f", filter.label()),
            }
        } else {
            "scanning\u{2026}".to_string()
        };
        let text = format!(" {text}");
        line(root, area, &[(text.as_str(), theme.status)]);
        return;
    }

    // Same three-column split as the file view: each side renders its own tree, aligned
    // row by row, with the marker column between. A name missing on one side leaves that
    // pane blank, so reading down one column tells you what that tree actually contains.
    let [left, marker, right] = pane_columns(area);
    for (side, pane) in [(SideIdx::LEFT, left), (SideIdx::RIGHT, right)] {
        let canvas = dir_pane(pane, view, &visible, top, cursor, side, theme);
        place(root, pane, canvas);
    }
    let canvas = dir_markers(marker, view, &visible, top, cursor, theme);
    place(root, marker, canvas);
}

/// One side of the directory tree.
///
/// Rows a side does not hold render as blank filler, which is what keeps the two panes
/// aligned: row `i` is the same entry in both, exactly as in the file view.
fn dir_pane(
    area: Rect,
    view: &DirView,
    visible: &[usize],
    top: usize,
    cursor: usize,
    side: SideIdx,
    theme: &Theme,
) -> Canvas {
    let mut canvas = Canvas::new(area.width, area.height);
    let width = area.width as usize;
    let rows = visible
        .iter()
        .enumerate()
        .skip(top)
        .take(area.height as usize);

    for (y, (offset, &row_index)) in (0u16..).zip(rows) {
        let row = &view.rows[row_index];
        let present = row.present[side.idx()];

        let side_kind = row.kind_on(side);
        let side_is_dir = side_kind == Some(EntryKind::Dir);

        let text = if present {
            let indent = "  ".repeat(row.depth as usize);
            // Only a row both sides agree is a directory can be folded, so the marker
            // follows `row.kind` while the trailing slash follows what this side holds.
            let fold = if row.kind.is_dir() {
                if view.is_collapsed(&row.rel) {
                    "\u{25b8} " // ▸ collapsed
                } else {
                    "\u{25be} " // ▾ expanded
                }
            } else {
                "  "
            };
            let name = format!("{}{}", row.name(), if side_is_dir { "/" } else { "" });
            let size = match (side_kind, row.sizes[side.idx()]) {
                (Some(EntryKind::File), Some(bytes)) => human_size(bytes),
                _ => String::new(),
            };
            pad_row(&format!(" {indent}{fold}{name}"), &size, width)
        } else {
            " ".repeat(width)
        };

        let mut style = if present {
            status_style(row.status, theme)
        } else {
            theme.filler
        };
        if offset == cursor {
            style = theme.cursor(style);
        }
        canvas.print(0, y, &text, area.width, style);
    }
    canvas
}

/// The column between the panes. With colour off this carries the entire diff.
fn dir_markers(
    area: Rect,
    view: &DirView,
    visible: &[usize],
    top: usize,
    cursor: usize,
    theme: &Theme,
) -> Canvas {
    let mut canvas = Canvas::new(area.width, area.height);
    let rows = visible
        .iter()
        .enumerate()
        .skip(top)
        .take(area.height as usize);

    for (y, (offset, &row_index)) in (0u16..).zip(rows) {
        let mut style = theme.marker;
        if offset == cursor {
            style = theme.cursor(style);
        }
        let text = format!(" {} ", view.rows[row_index].status.glyph());
        canvas.print(0, y, &text, area.width, style);
    }
    canvas
}

/// Lay `left` and `right` across `width` display columns with `right` flush to the end,
/// padding the gap so the row background reaches the pane edge.
///
/// Measured in display columns, not chars: a CJK filename is twice as wide as its char
/// count and would otherwise push the size column out of alignment.
fn pad_row(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let right_width = UnicodeWidthStr::width(right);
    let left = truncate_to_width(left, width.saturating_sub(right_width + 1));
    let gap = width.saturating_sub(UnicodeWidthStr::width(left.as_str()) + right_width + 1);
    format!("{left}{}{right} ", " ".repeat(gap))
}

/// Cut a string to at most `width` display columns, never splitting a grapheme.
fn truncate_to_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > width {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    out
}

fn status_style(status: EntryStatus, theme: &Theme) -> Style {
    match status {
        EntryStatus::Identical => theme.equal,
        EntryStatus::Differing => theme.changed,
        EntryStatus::OnlyIn(SideIdx::LEFT) => theme.removed,
        EntryStatus::OnlyIn(_) => theme.added,
        EntryStatus::TypeMismatch | EntryStatus::Cycle | EntryStatus::Unreadable(_) => {
            theme.warning
        }
        EntryStatus::Pending => theme.filler,
    }
}

fn draw_dir_status(
    root: &mut Root<'_>,
    area: Rect,
    view: &mut DirView,
    theme: &Theme,
    message: &str,
) {
    let (scanned, differing, done, filter) = (view.scanned, view.differing, view.done, view.filter);
    let errors = view.errors.len();
    let shown = view.visible().len();

    let mut parts = Vec::new();
    if !message.is_empty() {
        parts.push(message.to_string());
    }
    parts.push(if done {
        format!("{scanned} entries")
    } else {
        format!("scanning\u{2026} {scanned}")
    });
    parts.push(format!("{differing} differing"));
    if shown != scanned {
        parts.push(format!("{shown} shown"));
    }
    parts.push(format!("filter: {}", filter.label()));
    if errors > 0 {
        parts.push(format!("{errors} errors"));
    }
    parts.push("? help".to_string());
    parts.push("q quit".to_string());

    let text = format!(" {}", parts.join(" \u{b7} "));
    line(root, area, &[(text.as_str(), theme.status)]);
}

// ---------------------------------------------------------------------------------------
// File view
// ---------------------------------------------------------------------------------------

fn draw_file_header(root: &mut Root<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let [left, _, right] = pane_columns(area);
    let focus = view.focus;
    for (side, area) in [(SideIdx::LEFT, left), (SideIdx::RIGHT, right)] {
        let path = if side == SideIdx::LEFT {
            view.cmp.left.path()
        } else {
            view.cmp.right.path()
        };
        // A side with unsaved changes is marked, so nothing is edited invisibly.
        if view.dirty[side.idx()] {
            let text = truncate_left(&format!("\u{25cf} {}", path.display()), area.width as usize);
            line(root, area, &[(text.as_str(), theme.warning)]);
        } else {
            render_path(root, area, path, focus == side, theme);
        }
    }
}

fn render_path(
    root: &mut Root<'_>,
    area: Rect,
    path: &std::path::Path,
    focused: bool,
    theme: &Theme,
) {
    let style = if focused {
        theme.header_focus
    } else {
        theme.header
    };
    let text = truncate_left(&path.display().to_string(), area.width as usize);
    line(root, area, &[(text.as_str(), style)]);
}

/// Truncate from the left, so the filename always survives.
fn truncate_left(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let tail: String = s.chars().skip(s.chars().count() - keep).collect();
    format!("\u{2026}{tail}")
}

fn draw_file_body(root: &mut Root<'_>, area: Rect, view: &FileView, theme: &Theme, tab_width: u8) {
    let Some(alignment) = view.cmp.alignment.as_ref() else {
        draw_not_diffable(root, area, view, theme);
        return;
    };
    let text_sides = view.cmp.texts();
    let hex_sides = view
        .cmp
        .hex
        .zip(view.cmp.left.as_binary().zip(view.cmp.right.as_binary()));
    if text_sides.is_none() && hex_sides.is_none() {
        draw_not_diffable(root, area, view, theme);
        return;
    }

    // A banner steals one row off the top when there is something the rows cannot say.
    let body = match &view.cmp.note {
        Some(note) => {
            let banner = Rect {
                height: area.height.min(1),
                ..area
            };
            let text = format!(" {note}");
            line(root, banner, &[(text.as_str(), theme.warning)]);
            Rect {
                y: area.y + banner.height,
                height: area.height - banner.height,
                ..area
            }
        }
        None => area,
    };

    let [left, marker, right] = pane_columns(body);
    let panes = [(SideIdx::LEFT, left), (SideIdx::RIGHT, right)];

    if let Some((per_row, (left_bytes, right_bytes))) = hex_sides {
        // A hex row is a chunk of bytes rather than a line, but the row bookkeeping,
        // cursor and marker column are the same — that is the point of aligning bytes with
        // the same machinery as lines.
        let gutter = hex_gutter_width(view);
        for ((side, area), bytes) in panes.into_iter().zip([left_bytes, right_bytes]) {
            let pane = hex::HexPane {
                alignment,
                rows: &view.rows,
                bytes,
                side,
                top: view.top,
                cursor: view.cursor,
                h_scroll: view.h_scroll,
                gutter_width: gutter,
                per_row,
                theme,
                focused: view.focus == side,
            };
            place(root, area, pane.render(area.width, area.height));
        }
    } else if let Some((left_text, right_text)) = text_sides {
        for ((side, area), text) in panes.into_iter().zip([left_text, right_text]) {
            let pane = DiffPane {
                alignment,
                rows: &view.rows,
                matches: view.search.as_ref().map_or(&[][..], |s| &s.matches),
                current_match: view.search.as_ref().and_then(|s| s.current_match()),
                text,
                side,
                top: view.top,
                cursor: view.cursor,
                h_scroll: view.h_scroll,
                gutter_width: gutter_width(text),
                tab_width,
                theme,
                focused: view.focus == side,
            };
            place(root, area, pane.render(area.width, area.height));
        }
    }

    place(root, marker, markers(marker, view, theme));
}

/// The column between the panes: the only place the diff is legible with colour off.
fn markers(area: Rect, view: &FileView, theme: &Theme) -> Canvas {
    let mut canvas = Canvas::new(area.width, area.height);
    let Some(alignment) = view.cmp.alignment.as_ref() else {
        return canvas;
    };
    for y in 0..area.height {
        let index = view.top + y as usize;
        let Some(view_row) = view.rows.get(index).copied() else {
            break;
        };
        let on_cursor = index == view.cursor;
        // A fold covers only equal rows, so it never hides a marker.
        if view_row.is_fold() {
            let style = if on_cursor {
                theme.cursor(theme.fold)
            } else {
                theme.fold
            };
            canvas.print(0, y, "\u{22ef}  ", area.width, style);
            continue;
        }
        let Some(row) = alignment.row(view_row.anchor() as usize) else {
            break;
        };
        let glyph = match row.kind {
            RowKind::Equal => " ",
            RowKind::Changed | RowKind::Conflict => "\u{25ac}", // ▬
            RowKind::Unmatched => {
                if row.cells[SideIdx::LEFT.idx()].is_filler() {
                    "\u{25b6}" // ▶ only on the right
                } else {
                    "\u{25c0}" // ◀ only on the left
                }
            }
        };
        let style = if on_cursor {
            theme.cursor(theme.marker)
        } else {
            theme.marker
        };
        canvas.print(0, y, &format!(" {glyph} "), area.width, style);
    }
    canvas
}

fn draw_not_diffable(root: &mut Root<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let plain = |text: String| vec![(text, Style::new())];
    let mut lines = vec![plain(String::new())];
    if let Some(note) = &view.cmp.note {
        lines.push(vec![(note.clone(), theme.warning)]);
        lines.push(plain(String::new()));
    }
    for (label, content) in [("left ", &view.cmp.left), ("right", &view.cmp.right)] {
        lines.push(plain(format!(
            "{label}  {}  {}",
            content.path().display(),
            describe(content)
        )));
    }
    lines.push(plain(String::new()));
    let verdict = if view.cmp.differ {
        "the two files differ"
    } else {
        "the two files are identical"
    };
    lines.push(vec![(verdict.to_string(), theme.status)]);

    panel(root, area, Some(" not comparable line by line "));
    paragraph(root, area.inner(), &lines, TextWrap::Word);
}

fn describe(content: &FileContent) -> String {
    match content {
        FileContent::Text(t) => format!("{} lines", t.line_count()),
        FileContent::Binary { size, .. } => format!("binary, {}", human_size(*size)),
        FileContent::TooLarge { size, .. } => format!("{}, too large", human_size(*size)),
        FileContent::Unreadable { message, .. } => message.clone(),
    }
}

fn draw_file_status(
    root: &mut Root<'_>,
    area: Rect,
    view: &FileView,
    theme: &Theme,
    message: &str,
) {
    let rows = view.row_count();
    let mut parts = Vec::new();
    if !message.is_empty() {
        parts.push(message.to_string());
    }

    if rows > 0 {
        parts.push(format!("row {}/{}", view.cursor + 1, rows));
    }
    if let Some(alignment) = view.cmp.alignment.as_ref() {
        let anchor = view.cursor_row().unwrap_or(0) as usize;
        // A hex row is a chunk of bytes, so the useful position there is its offset rather
        // than a line number.
        let at = |side: SideIdx| match alignment.cell(anchor, side) {
            _ if view.cursor_is_fold() => "\u{22ef}".to_string(),
            Cell::Line(n) => match view.cmp.hex {
                Some(per_row) => format!("{:#x}", hex::row_offset(n.0, per_row)),
                None => n.display().to_string(),
            },
            Cell::Filler => "-".to_string(),
        };
        parts.push(match view.cmp.hex {
            Some(_) => format!("L@{} R@{}", at(SideIdx::LEFT), at(SideIdx::RIGHT)),
            None => format!("L:{} R:{}", at(SideIdx::LEFT), at(SideIdx::RIGHT)),
        });

        let stats = alignment.stats();
        if alignment.is_identical() && !view.cmp.differ {
            parts.push("identical".to_string());
        } else {
            parts.push(format!(
                "+{} -{} ~{}",
                stats.inserted, stats.deleted, stats.changed
            ));
            parts.push(format!("{} hunks", alignment.hunks().len()));
        }
    }
    if view.h_scroll > 0 {
        parts.push(format!("col +{}", view.h_scroll));
    }
    if view.is_dirty() {
        parts.push(format!(
            "modified: {}",
            match (view.dirty[0], view.dirty[1]) {
                (true, true) => "both",
                (true, false) => "left",
                _ => "right",
            }
        ));
    }
    if !view.undo_log.is_empty() {
        parts.push(format!("{} to undo", view.undo_log.len()));
    }
    if let Some(search) = view.search.as_ref() {
        parts.push(if search.matches.is_empty() {
            format!("/{} no match", search.query)
        } else {
            format!(
                "/{} {}/{}",
                search.query,
                search.current + 1,
                search.matches.len()
            )
        });
    }
    if view.diff_opts.whitespace != crate::diff::Whitespace::Exact {
        parts.push(format!("ws: {}", view.diff_opts.whitespace.label()));
    }
    if let Some(context) = view.context {
        let hidden = view.hidden_rows();
        parts.push(if hidden > 0 {
            format!("folded -{hidden} (ctx {context})")
        } else {
            format!("fold on (ctx {context})")
        });
    }
    parts.push("? help".to_string());
    parts.push("q quit".to_string());

    let text = format!(" {}", parts.join(" \u{b7} "));
    line(root, area, &[(text.as_str(), theme.status)]);
}

/// Offset-gutter width for a hex comparison. Shared by the width fit and the renderer so
/// the two cannot disagree about how much room the row has.
fn hex_gutter_width(view: &FileView) -> u16 {
    let bytes = view
        .cmp
        .left
        .as_binary()
        .map_or(0, <[u8]>::len)
        .max(view.cmp.right.as_binary().map_or(0, <[u8]>::len));
    hex::gutter_width(bytes as u64)
}

/// A small centred panel announcing that the two sides match.
///
/// Shown on opening and cleared by the first keystroke: two identical files render as a
/// screen of unremarkable text, which looks the same as a diff that has not loaded.
fn draw_identical_notice(root: &mut Root<'_>, area: Rect, view: &FileView, theme: &Theme) {
    // How big the thing that matched is — a line count for text, a size for bytes.
    let size = match (view.cmp.hex, view.cmp.texts()) {
        (Some(_), _) => {
            let bytes = view.cmp.left.as_binary().map_or(0, <[u8]>::len);
            human_size(bytes as u64)
        }
        (None, Some((left, _))) => {
            let n = left.line_count();
            format!("{n} line{}", if n == 1 { "" } else { "s" })
        }
        _ => String::new(),
    };

    let mut lines = vec![vec![("  \u{2713}  identical".to_string(), theme.identical)]];
    // `--ignore-whitespace` and the CRLF case both reach this overlay, and there the
    // headline alone would overstate it.
    if let Some(note) = view.cmp.note.as_ref() {
        lines.push(vec![(format!("     {note}"), theme.warning)]);
    }
    lines.push(vec![(
        if size.is_empty() {
            "     press any key".to_string()
        } else {
            format!("     {size} \u{00b7} press any key")
        },
        theme.status,
    )]);

    let width = lines
        .iter()
        .map(|parts| {
            parts
                .iter()
                .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(20)
        .saturating_add(4)
        .min(area.width as usize) as u16;
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };

    panel(root, popup, None);
    paragraph(root, popup.inner(), &lines, TextWrap::None);
}

fn draw_help(root: &mut Root<'_>, area: Rect, theme: &Theme, dir_mode: bool) {
    // Grouped rather than one key per line: the list has to fit a short terminal, and a
    // clipped help screen is worse than a terse one.
    const FILE_HELP: &[(&str, &str)] = &[
        ("j k / arrows", "move down / up"),
        ("PgDn PgUp Ctrl-d Ctrl-u", "page / half page"),
        ("g G", "first / last row"),
        ("n p", "next / previous hunk"),
        ("h l   H L   0", "scroll horizontally"),
        ("/   N P", "search, next / previous match"),
        ("> <", "copy the hunk to the other side"),
        ("u   s", "undo, save changed files"),
        ("z   Enter", "fold / unfold, expand one fold"),
        ("Tab", "switch focused side"),
        ("?   Esc   q", "help, back, quit"),
    ];
    const DIR_HELP: &[(&str, &str)] = &[
        ("j k / arrows", "move down / up"),
        ("Ctrl-d Ctrl-u", "half page"),
        ("PgDn PgUp Space b", "page"),
        ("g G", "first / last entry"),
        ("n p", "next / previous difference"),
        ("Enter", "open a diff, or fold a directory"),
        ("l h / arrows", "expand / collapse"),
        ("f", "cycle filter: all / changes / differing"),
        ("r", "rescan"),
        ("?", "toggle this help"),
        ("Esc  q  Ctrl-C", "quit"),
    ];

    let help = if dir_mode { DIR_HELP } else { FILE_HELP };
    let width = 58.min(area.width);
    let height = (help.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines: Vec<Vec<(String, Style)>> = help
        .iter()
        .map(|(keys, what)| {
            vec![
                (format!(" {keys:<24}"), theme.header_focus),
                ((*what).to_string(), Style::new()),
            ]
        })
        .collect();

    panel(root, popup, Some(" keys "));
    paragraph(root, popup.inner(), &lines, TextWrap::None);
}

/// Header, body and status bar: a row each for the first and last, the rest between.
fn screen_rows(area: Rect) -> [Rect; 3] {
    let header = area.height.min(1);
    let status = u16::from(area.height >= 2);
    let body = area.height - header - status;
    [
        Rect {
            height: header,
            ..area
        },
        Rect {
            y: area.y + header,
            height: body,
            ..area
        },
        Rect {
            y: area.y + header + body,
            height: status,
            ..area
        },
    ]
}

/// Split an area into left pane, marker column, right pane.
///
/// The panes share what the marker leaves, and an odd column goes to the left one.
fn pane_columns(area: Rect) -> [Rect; 3] {
    let marker = MARKER_WIDTH.min(area.width);
    let panes = area.width - marker;
    let left = panes.div_ceil(2);
    let right = panes - left;
    [
        Rect {
            width: left,
            ..area
        },
        Rect {
            x: area.x + left,
            width: marker,
            ..area
        },
        Rect {
            x: area.x + left + marker,
            width: right,
            ..area
        },
    ]
}

/// Gutter wide enough for the largest line number, with a sensible floor.
fn gutter_width(text: &TextFile) -> u16 {
    let digits = text.line_count().max(1).to_string().len() as u16;
    digits.max(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_truncate_from_the_left_so_the_filename_survives() {
        assert_eq!(truncate_left("/a/b/c.rs", 20), "/a/b/c.rs");
        assert_eq!(
            truncate_left("/very/long/path/model.rs", 12),
            "\u{2026}th/model.rs"
        );
        assert_eq!(truncate_left("abc", 0), "");
    }

    #[test]
    fn pane_columns_survive_a_tiny_terminal() {
        // Two columns cannot hold two panes plus a 3-wide marker; the split must still
        // produce valid rects rather than underflowing.
        let [left, marker, right] = pane_columns(Rect::new(0, 0, 2, 1));
        assert!(left.width + marker.width + right.width <= 2);
    }

    #[test]
    fn an_odd_column_goes_to_the_left_pane() {
        let [left, marker, right] = pane_columns(Rect::new(0, 0, 30, 1));
        assert_eq!(
            (left.width, marker.x, right.x, right.width),
            (14, 14, 17, 13)
        );
    }

    #[test]
    fn the_body_is_what_the_header_and_status_bar_leave() {
        let [header, body, status] = screen_rows(Rect::new(0, 0, 10, 7));
        assert_eq!((header.y, header.height), (0, 1));
        assert_eq!((body.y, body.height), (1, 5));
        assert_eq!((status.y, status.height), (6, 1));

        // Too short for all three: nothing underflows, and the header survives longest.
        let [header, body, status] = screen_rows(Rect::new(0, 0, 10, 1));
        assert_eq!((header.height, body.height, status.height), (1, 0, 0));
    }
}

#[cfg(test)]
mod render_tests {
    use crate::app::{App, Options};
    use crate::diff::{DEFAULT_TIMEOUT, DiffOptions, compare_loaded};
    use crate::text::{FileContent, decode};
    use crate::theme::Theme;
    use crate::{Inputs, app, testing};

    fn content(name: &str, body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new(name),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    fn app_with(left: FileContent, right: FileContent) -> App {
        let opts = Options {
            diff: DiffOptions {
                timeout: DEFAULT_TIMEOUT,
                ..DiffOptions::default()
            },
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
            context: None,
            hex_width_pinned: false,
        };
        // Build the view directly: App::new goes through the filesystem.
        let cmp = compare_loaded(left, right, &opts.diff);
        let mut a = App::new(
            Inputs::Files {
                left: "a".into(),
                right: "b".into(),
            },
            opts,
            Theme::no_color(),
        )
        .unwrap_or_else(|_| unreachable!("only reached when both paths exist"));
        a.views = vec![app::View::File(Box::new(app::FileView::new(cmp)))];
        a
    }

    fn frame(app: &mut App, width: u16, height: u16) -> Vec<String> {
        testing::render(app, width, height).lines()
    }

    #[test]
    fn side_by_side_frame_aligns_gutters_and_markers() {
        let mut app = app_with(
            content("a.txt", "same\nold\ntail\n"),
            content("b.txt", "same\nnew\nextra\ntail\n"),
        );
        let lines = frame(&mut app, 30, 7);

        // Row 0 is the header, rows 1..5 are the diff, row 6 is the status bar.
        // Left gutter occupies columns 0..3 with text at 4; the marker column is centred
        // at column 15; the right gutter is 17..20 with text at 21.
        assert_eq!(lines[0], "a.txt            b.txt");
        assert_eq!(lines[1], "  1 same           1 same");
        assert_eq!(lines[2], "  2 old        \u{25ac}   2 new");
        // similar folds "old"->"new" and the added "extra" into one replace op, but the
        // row with a filler on the left is still an insertion, and the marker says so.
        assert_eq!(lines[3], "               \u{25b6}   3 extra");
        assert_eq!(lines[4], "  3 tail           4 tail");
        assert!(lines[6].contains("row 1/4"), "status was {:?}", lines[6]);
    }

    #[test]
    fn a_deleted_line_marks_the_left_and_fills_the_right() {
        let mut app = app_with(content("a.txt", "keep\ndrop\n"), content("b.txt", "keep\n"));
        let lines = frame(&mut app, 30, 5);
        assert_eq!(lines[1], "  1 keep           1 keep");
        // Only on the left: the right side is filler, so its gutter and text stay blank
        // and the row ends at the marker.
        assert_eq!(lines[2], "  2 drop       \u{25c0}");
    }

    #[test]
    fn a_binary_side_renders_the_not_comparable_pane() {
        let mut app = app_with(
            content("a.txt", "text\n"),
            FileContent::Binary {
                path: "b.bin".into(),
                size: 2048,
                reason: crate::text::BinaryReason::Nul,
                bytes: vec![0; 2048],
            },
        );
        let lines = frame(&mut app, 46, 12);
        let joined = lines.join("\n");
        assert!(joined.contains("not comparable line by line"), "{joined}");
        assert!(joined.contains("right  b.bin  binary"), "{joined}");
        assert!(joined.contains("binary, 2.0 KiB"), "{joined}");
    }

    #[test]
    fn a_two_column_terminal_does_not_panic() {
        let mut app = app_with(content("a.txt", "x\n"), content("b.txt", "y\n"));
        let _ = frame(&mut app, 2, 3);
        let _ = frame(&mut app, 1, 1);
    }
}

#[cfg(test)]
mod dir_render_tests {
    use crate::app::{App, DirFilter, Options};
    use crate::diff::DiffOptions;
    use crate::dirdiff::{DirEntryRow, EntryKind, EntryStatus, ScanEvent};
    use crate::model::SideIdx;
    use crate::theme::Theme;
    use crate::{Inputs, app, testing};

    /// `present` is derived from the status, exactly as the scanner does it -- a fixture
    /// that claims both sides hold a one-sided entry would hide a real rendering bug.
    fn row(rel: &str, depth: u16, kind: EntryKind, status: EntryStatus) -> DirEntryRow {
        let present = match status {
            EntryStatus::OnlyIn(SideIdx::LEFT) => [true, false, false],
            EntryStatus::OnlyIn(_) => [false, true, false],
            _ => [true, true, false],
        };
        let size = matches!(kind, EntryKind::File).then_some(2048);
        DirEntryRow {
            rel: std::path::PathBuf::from(rel),
            depth,
            kind,
            kinds: [present[0].then_some(kind), present[1].then_some(kind), None],
            present,
            status,
            sizes: [
                present[0].then_some(size).flatten(),
                present[1].then_some(size).flatten(),
                None,
            ],
        }
    }

    /// An app holding a directory view populated without touching the filesystem.
    fn dir_app(rows: Vec<DirEntryRow>) -> App {
        let opts = Options {
            diff: DiffOptions::default(),
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
            context: None,
            hex_width_pinned: false,
        };
        // Point the scan at a directory that exists but is empty of interest; the rows
        // below replace whatever it finds.
        let mut app = App::new(
            Inputs::Dirs {
                left: std::env::temp_dir(),
                right: std::env::temp_dir(),
            },
            opts,
            Theme::no_color(),
        )
        .expect("temp dir exists");

        if let Some(dir) = app.active_dir() {
            dir.stop_scan_for_test();
            for r in rows {
                dir.apply_for_test(ScanEvent::Entry(Box::new(r)));
            }
            dir.apply_for_test(ScanEvent::Done { cancelled: false });
        }
        app
    }

    fn frame(app: &mut App, width: u16, height: u16) -> Vec<String> {
        testing::render(app, width, height).lines()
    }

    fn sample() -> Vec<DirEntryRow> {
        vec![
            row("same.txt", 0, EntryKind::File, EntryStatus::Identical),
            row("changed.txt", 0, EntryKind::File, EntryStatus::Differing),
            row(
                "gone.txt",
                0,
                EntryKind::File,
                EntryStatus::OnlyIn(SideIdx::LEFT),
            ),
            row("sub", 0, EntryKind::Dir, EntryStatus::Differing),
            row("sub/b.txt", 1, EntryKind::File, EntryStatus::Differing),
        ]
    }

    #[test]
    fn the_tree_shows_status_glyphs_indentation_and_fold_markers() {
        let mut app = dir_app(sample());
        let lines = frame(&mut app, 66, 8);

        // Row 0 is the header (both roots). Each side renders its own tree with its own
        // size column, and the marker column sits between them.
        assert_eq!(
            lines[1],
            "   same.txt             2.0 KiB       same.txt            2.0 KiB"
        );
        assert_eq!(
            lines[2],
            "   changed.txt          2.0 KiB  \u{25ac}    changed.txt         2.0 KiB"
        );
        // Only on the left: the right pane is blank, which is the whole point of the
        // mirrored layout — reading down one column shows what that tree holds.
        assert_eq!(lines[3], "   gone.txt             2.0 KiB  \u{25c0}");
        // A directory carries a fold marker and a trailing slash, and no size.
        assert_eq!(
            lines[4],
            " \u{25be} sub/                          \u{25ac}  \u{25be} sub/"
        );
        // Its child is indented one level deeper on both sides.
        assert_eq!(
            lines[5],
            "     b.txt              2.0 KiB  \u{25ac}      b.txt             2.0 KiB"
        );
    }

    #[test]
    fn collapsing_swaps_the_fold_marker_and_hides_children() {
        let mut app = dir_app(sample());
        let dir = app.active_dir().unwrap();
        dir.collapse_for_test(std::path::Path::new("sub"));

        let lines = frame(&mut app, 66, 8);
        // The fold marker flips to ▸ on both sides.
        assert_eq!(
            lines[4],
            " \u{25b8} sub/                          \u{25ac}  \u{25b8} sub/"
        );
        // b.txt is gone; row 5 is blank.
        assert_eq!(lines[5], "");
    }

    #[test]
    fn a_type_mismatch_shows_each_side_as_what_it_actually_is() {
        // A directory on the left facing a regular file on the right. Rendering both as
        // directories would be a lie, and would make the row look expandable.
        let mut rows = vec![row("clash", 0, EntryKind::Other, EntryStatus::TypeMismatch)];
        rows[0].kinds = [Some(EntryKind::Dir), Some(EntryKind::File), None];
        rows[0].sizes = [None, Some(2048), None];

        let mut app = dir_app(rows);
        let lines = frame(&mut app, 66, 4);

        // Left: a trailing slash and no size. Right: no slash, and its size.
        // Neither side gets a fold marker, because the row is not expandable.
        assert_eq!(
            lines[1],
            "   clash/                        \u{2260}    clash               2.0 KiB"
        );
    }

    #[test]
    fn the_status_bar_reports_counts_and_the_active_filter() {
        let mut app = dir_app(sample());
        let lines = frame(&mut app, 70, 8);
        let status = lines.last().unwrap();
        assert!(status.contains("5 entries"), "{status}");
        assert!(status.contains("4 differing"), "{status}");
        assert!(status.contains("filter: all"), "{status}");
    }

    #[test]
    fn an_empty_filter_result_explains_itself_instead_of_showing_nothing() {
        let mut app = dir_app(vec![row(
            "same.txt",
            0,
            EntryKind::File,
            EntryStatus::Identical,
        )]);
        app.active_dir().unwrap().filter = DirFilter::DifferingOnly;
        app.active_dir().unwrap().mark_dirty_for_test();

        let lines = frame(&mut app, 60, 5);
        assert!(lines[1].contains("nothing to show"), "{:?}", lines[1]);
        assert!(lines[1].contains("press f"), "{:?}", lines[1]);
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let mut app = dir_app(sample());
        let _ = frame(&mut app, 4, 3);
        let _ = frame(&mut app, 1, 1);
    }

    #[test]
    fn the_help_overlay_shows_directory_keys_in_directory_mode() {
        let mut app = dir_app(sample());
        app.help_visible = true;
        let joined = frame(&mut app, 60, 18).join("\n");
        assert!(joined.contains("cycle filter"), "{joined}");
        assert!(joined.contains("rescan"), "{joined}");
        assert!(!joined.contains("switch focused side"), "{joined}");
    }

    #[test]
    fn the_help_overlay_shows_file_keys_in_file_mode() {
        let mut app = dir_app(sample());
        app.views.push(app::View::File(Box::new(app::FileView::new(
            crate::diff::compare_loaded(
                crate::text::FileContent::Text(Box::new(crate::text::decode(
                    std::path::Path::new("a"),
                    b"x\n".to_vec(),
                    None,
                ))),
                crate::text::FileContent::Text(Box::new(crate::text::decode(
                    std::path::Path::new("b"),
                    b"y\n".to_vec(),
                    None,
                ))),
                &DiffOptions::default(),
            ),
        ))));
        app.help_visible = true;
        let joined = frame(&mut app, 60, 18).join("\n");
        assert!(joined.contains("switch focused side"), "{joined}");
        assert!(!joined.contains("cycle filter"), "{joined}");
    }
}

#[cfg(test)]
mod cursor_tests {
    use crate::app::{App, Options, View};
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::style::Color;
    use crate::testing::{self, Screen};
    use crate::text::{FileContent, decode};
    use crate::theme::Theme;
    use crate::{Inputs, app};

    fn content(name: &str, body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new(name),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    /// A file view on the real (coloured) theme, so cursor styling is observable.
    fn file_app(left: &str, right: &str) -> App {
        let opts = Options {
            diff: DiffOptions::default(),
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
            context: None,
            hex_width_pinned: false,
        };
        let cmp = compare_loaded(content("a", left), content("b", right), &opts.diff);
        let mut app = App::new(
            Inputs::Dirs {
                left: std::env::temp_dir(),
                right: std::env::temp_dir(),
            },
            opts,
            Theme::dark(),
        )
        .expect("temp dir exists");
        app.views = vec![View::File(Box::new(app::FileView::new(cmp)))];
        // These tests are about cursor styling on identical rows, so dismiss the overlay
        // that would otherwise sit on top of them.
        app.active_file().unwrap().notice = false;
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> Screen {
        testing::render(app, width, height)
    }

    fn row_backgrounds(screen: &Screen, y: u16, width: u16) -> Vec<Option<Color>> {
        (0..width).map(|x| screen.bg(x, y)).collect()
    }

    #[test]
    fn the_selected_row_is_highlighted_across_the_full_width() {
        let mut app = file_app("one\ntwo\nthree\n", "one\ntwo\nthree\n");
        let screen = render(&mut app, 40, 6);

        // Body starts at y=1. The cursor sits on the first row.
        let selected = row_backgrounds(&screen, 1, 40);
        let plain = row_backgrounds(&screen, 2, 40);

        assert!(
            selected.iter().all(|bg| *bg == selected[0]),
            "the highlight must be uniform across the row, including the gutters and the \
             marker column: {selected:?}"
        );
        assert_ne!(
            selected[0], plain[0],
            "the selected row must differ from an unselected one"
        );
        assert!(selected[0].is_some(), "the selected row needs a background");
    }

    #[test]
    fn the_highlight_covers_both_panes_and_the_marker_column() {
        let mut app = file_app("one\ntwo\n", "one\ntwo\n");
        let screen = render(&mut app, 40, 5);
        let selected = row_backgrounds(&screen, 1, 40);

        // Left pane, marker column and right pane all carry it -- the cursor is a property
        // of the row, not of whichever pane happens to have focus.
        let left = selected[2];
        let marker = selected[20];
        let right = selected[37];
        assert_eq!(left, marker);
        assert_eq!(marker, right);
    }

    #[test]
    fn moving_the_cursor_moves_the_highlight() {
        let mut app = file_app("one\ntwo\nthree\n", "one\ntwo\nthree\n");
        let before = render(&mut app, 40, 6);
        let first_row_bg = before.bg(2, 1);

        app.active_file().unwrap().cursor = 1;
        let after = render(&mut app, 40, 6);

        assert_ne!(
            after.bg(2, 1),
            first_row_bg,
            "row 1 should no longer be highlighted"
        );
        assert_eq!(
            after.bg(2, 2),
            first_row_bg,
            "row 2 should now carry the highlight"
        );
    }

    #[test]
    fn a_changed_row_keeps_its_colour_when_selected() {
        // The reason the cursor lifts the background instead of reversing it: a selected
        // changed line must still look changed.
        let mut app = file_app("same\nold\n", "same\nnew\n");
        let mut screen = render(&mut app, 40, 5);
        let equal_selected = screen.bg(2, 1);

        app.active_file().unwrap().cursor = 1;
        screen = render(&mut app, 40, 5);
        let changed_selected = screen.bg(2, 2);

        assert_ne!(
            equal_selected, changed_selected,
            "a selected changed row must not look like a selected equal row"
        );
    }
}

#[cfg(test)]
mod hex_render_tests {
    use crate::app::{App, Options, View};
    use crate::diff::{DiffOptions, compare_loaded};
    use crate::testing::{self, Screen};
    use crate::text::{BinaryReason, FileContent};
    use crate::theme::Theme;
    use crate::{Inputs, app};

    fn binary(name: &str, bytes: &[u8]) -> FileContent {
        FileContent::Binary {
            path: name.into(),
            size: bytes.len() as u64,
            reason: BinaryReason::Nul,
            bytes: bytes.to_vec(),
        }
    }

    fn hex_app(left: &[u8], right: &[u8], per_row: u32) -> App {
        let opts = Options {
            diff: DiffOptions {
                hex_bytes_per_row: per_row,
                ..DiffOptions::default()
            },
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
            context: None,
            // These tests assert exact columns, so the row width must not be refitted to
            // the test terminal.
            hex_width_pinned: true,
        };
        let cmp = compare_loaded(binary("a.bin", left), binary("b.bin", right), &opts.diff);
        let mut app = App::new(
            Inputs::Dirs {
                left: std::env::temp_dir(),
                right: std::env::temp_dir(),
            },
            opts,
            Theme::dark(),
        )
        .expect("temp dir exists");
        app.views = vec![View::File(Box::new(app::FileView::new(cmp)))];
        let view = app.active_file().unwrap();
        // Layout tests want the panes, not the overlay that appears on identical input.
        view.notice = false;
        view.hex_pinned = true;
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> (Vec<String>, Screen) {
        let screen = testing::render(app, width, height);
        (screen.lines(), screen)
    }

    #[test]
    fn two_binaries_render_as_side_by_side_hex() {
        let mut app = hex_app(b"AB\x00\xffZZ", b"AB\x00\xffYY", 4);
        let (lines, _) = render(&mut app, 66, 6);

        assert_eq!(lines[0], "a.bin                              b.bin");
        // Offset gutter in hex, then the bytes, then the printable-ASCII pane. The short
        // final row is padded so both sides stay column-aligned.
        assert_eq!(
            lines[1],
            "0000 41 42 00 ff  |AB..|           0000 41 42 00 ff  |AB..|"
        );
        assert_eq!(
            lines[2],
            "0004 5a 5a        |ZZ  |         \u{25ac} 0004 59 59        |YY  |"
        );
    }

    #[test]
    fn only_the_differing_bytes_are_emphasised() {
        // Byte 1 of the second row differs; byte 0 does not.
        let mut app = hex_app(b"abcdXY", b"abcdXZ", 4);
        let (lines, screen) = render(&mut app, 66, 5);
        assert_eq!(
            lines[2],
            "0004 58 59        |XY  |         \u{25ac} 0004 58 5a        |XZ  |"
        );

        // The left pane starts at column 0; gutter is 4 wide plus a space, so the hex
        // area begins at column 5. Byte 0 is columns 5..7, byte 1 is 8..10.
        let bg = |x: u16| screen.bg(x, 2);
        let row_bg = bg(5);
        assert_eq!(bg(6), row_bg, "byte 0 matches, so it keeps the row colour");
        assert_ne!(bg(8), row_bg, "byte 1 differs and must stand out");
        assert_ne!(bg(9), row_bg);
    }

    #[test]
    fn the_ascii_column_is_emphasised_too() {
        let mut app = hex_app(b"abcdXY", b"abcdXZ", 4);
        let (lines, screen) = render(&mut app, 66, 5);
        // Find the 'Y' in the left pane's ascii block on row 2.
        let y_col = lines[2].find('Y').unwrap() as u16;
        let bg = |x: u16| screen.bg(x, 2);
        assert_ne!(
            bg(y_col),
            bg(y_col - 1),
            "the changed ascii character stands out"
        );
    }

    #[test]
    fn a_narrow_pane_drops_the_ascii_block_rather_than_truncating_hex() {
        let mut app = hex_app(b"abcdefgh", b"abcdefgh", 4);
        let (lines, _) = render(&mut app, 40, 5);
        assert!(
            !lines[1].contains('|'),
            "ascii should be dropped: {:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("61 62 63 64"),
            "hex must survive: {:?}",
            lines[1]
        );
    }

    #[test]
    fn a_longer_file_shows_its_tail_against_an_empty_pane() {
        let mut app = hex_app(b"abcdefgh", b"abcd", 4);
        let (lines, _) = render(&mut app, 66, 5);
        // Row 1 exists only on the left.
        assert!(lines[2].starts_with("0004 65 66 67 68"), "{:?}", lines[2]);
        assert!(lines[2].trim_end().ends_with('\u{25c0}'), "{:?}", lines[2]);
    }

    #[test]
    fn identical_binaries_show_no_markers() {
        let mut app = hex_app(b"same", b"same", 4);
        let (lines, _) = render(&mut app, 66, 4);
        assert!(!lines[1].contains('\u{25ac}'));
        assert!(lines.last().unwrap().contains("identical"));
    }

    #[test]
    fn a_long_identical_run_folds_in_hex_too() {
        // Folding is a view transform over the alignment, so it works on bytes unchanged.
        let mut left = vec![0u8; 400];
        left[0] = 1;
        let mut right = left.clone();
        right[0] = 2;

        let mut app = hex_app(&left, &right, 4);
        app.active_file().unwrap().context = Some(1);
        // Rebuild through the public toggle path.
        app.active_file().unwrap().toggle_fold();
        app.active_file().unwrap().toggle_fold();

        let (lines, _) = render(&mut app, 66, 8);
        let joined = lines.join("\n");
        assert!(
            joined.contains("identical bytes"),
            "expected a fold summary in bytes, got:\n{joined}"
        );
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let mut app = hex_app(b"abcd", b"abce", 4);
        let _ = render(&mut app, 6, 3);
        let _ = render(&mut app, 1, 1);
    }
}

#[cfg(test)]
mod notice_tests {
    use crate::app::{App, Options, View};
    use crate::diff::{DiffOptions, Whitespace, compare_loaded};
    use crate::key::{KeyCode, KeyEvent};
    use crate::testing;
    use crate::text::{BinaryReason, FileContent, decode};
    use crate::theme::Theme;
    use crate::{Inputs, app};

    fn text(body: &str) -> FileContent {
        FileContent::Text(Box::new(decode(
            std::path::Path::new("f"),
            body.as_bytes().to_vec(),
            None,
        )))
    }

    fn binary(bytes: &[u8]) -> FileContent {
        FileContent::Binary {
            path: "b".into(),
            size: bytes.len() as u64,
            reason: BinaryReason::Nul,
            bytes: bytes.to_vec(),
        }
    }

    fn app_for(left: FileContent, right: FileContent, whitespace: Whitespace) -> App {
        let opts = Options {
            diff: DiffOptions {
                whitespace,
                hex_bytes_per_row: 4,
                ..DiffOptions::default()
            },
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
            context: None,
            hex_width_pinned: false,
        };
        let cmp = compare_loaded(left, right, &opts.diff);
        let mut app = App::new(
            Inputs::Dirs {
                left: std::env::temp_dir(),
                right: std::env::temp_dir(),
            },
            opts,
            Theme::dark(),
        )
        .expect("temp dir exists");
        app.views = vec![View::File(Box::new(app::FileView::configured(
            cmp,
            None,
            DiffOptions {
                whitespace,
                ..DiffOptions::default()
            },
        )))];
        app
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        testing::render(app, width, height).text()
    }

    #[test]
    fn identical_files_announce_themselves_on_opening() {
        let mut app = app_for(
            text("same\nhere\n"),
            text("same\nhere\n"),
            Whitespace::Exact,
        );
        let out = screen(&mut app, 50, 12);
        assert!(out.contains('\u{2713}'), "expected a tick:\n{out}");
        assert!(out.contains("identical"), "{out}");
        assert!(out.contains("2 lines"), "{out}");
        assert!(out.contains("press any key"), "{out}");
    }

    #[test]
    fn the_notice_is_green() {
        let mut app = app_for(text("a\n"), text("a\n"), Whitespace::Exact);
        let rendered = testing::render(&mut app, 50, 12);

        let (x, y) = (0..50u16)
            .flat_map(|x| (0..12u16).map(move |y| (x, y)))
            .find(|&(x, y)| rendered.cell(x, y).symbol == "\u{2713}")
            .expect("the tick should be on screen");
        assert_eq!(
            rendered.fg(x, y),
            Theme::dark().identical.fg,
            "the good news should be green"
        );
    }

    #[test]
    fn differing_files_get_no_notice() {
        let mut app = app_for(text("one\n"), text("two\n"), Whitespace::Exact);
        let out = screen(&mut app, 50, 12);
        assert!(
            !out.contains('\u{2713}'),
            "a real diff must not claim to match:\n{out}"
        );
    }

    #[test]
    fn identical_binaries_report_their_size_instead_of_a_line_count() {
        let mut app = app_for(
            binary(&[0u8; 2048]),
            binary(&[0u8; 2048]),
            Whitespace::Exact,
        );
        let out = screen(&mut app, 50, 12);
        assert!(out.contains("2.0 KiB"), "{out}");
        assert!(out.contains("identical"), "{out}");
    }

    #[test]
    fn a_whitespace_only_match_carries_the_caveat() {
        // The headline says identical; the caveat says why. Both matter.
        let mut app = app_for(text("a  b\n"), text("a b\n"), Whitespace::IgnoreChanges);
        let out = screen(&mut app, 60, 12);
        assert!(out.contains('\u{2713}'), "{out}");
        assert!(out.contains("apart from whitespace"), "{out}");
    }

    #[test]
    fn the_first_keystroke_clears_it_and_still_does_its_job() {
        let mut app = app_for(text("a\nb\nc\n"), text("a\nb\nc\n"), Whitespace::Exact);
        assert!(app.active_file().unwrap().notice);

        // 'j' dismisses the notice *and* moves the cursor: it is informational, not modal.
        app.handle_key(KeyEvent::new(KeyCode::Char('j')));
        assert!(!app.active_file().unwrap().notice);
        assert_eq!(app.active_file().unwrap().cursor, 1);

        let out = screen(&mut app, 50, 12);
        assert!(!out.contains('\u{2713}'));
    }

    #[test]
    fn escape_dismisses_and_leaves_in_one_press() {
        // Opening an identical file from the directory view and pressing Esc should go
        // straight back, not eat the keystroke on the overlay.
        let mut app = app_for(text("a\n"), text("a\n"), Whitespace::Exact);
        app.handle_key(KeyEvent::new(KeyCode::Esc));
        assert!(app.should_quit, "Esc on the only view should still quit");
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let mut app = app_for(text("a\n"), text("a\n"), Whitespace::Exact);
        let _ = screen(&mut app, 8, 4);
        let _ = screen(&mut app, 1, 1);
    }
}
