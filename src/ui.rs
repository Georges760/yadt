//! Layout and rendering. Free functions taking a `Frame`, per the house pattern.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, DirView, FileView, View};
use crate::diff::human_size;
use crate::dirdiff::{EntryKind, EntryStatus};
use crate::model::{Cell, RowKind, SideIdx};
use crate::pane::DiffPane;
use crate::text::{FileContent, TextFile};
use crate::theme::Theme;

/// Width of the column between the two panes.
const MARKER_WIDTH: u16 = 3;

/// Takes `&mut App` because scroll clamping happens here — doing it anywhere else leaves
/// one frame of garbage after a resize or a view push.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(area);

    let (header, body, status) = (chunks[0], chunks[1], chunks[2]);
    let theme = app.theme;
    let tab_width = app.opts.tab_width;
    let message = app.status.clone();

    match app.views.last_mut() {
        Some(View::File(view)) => {
            view.clamp(body);
            draw_file_header(frame, header, view, &theme);
            draw_file_body(frame, body, view, &theme, tab_width);
            draw_file_status(frame, status, view, &theme);
        }
        Some(View::Dir(view)) => {
            view.clamp(body);
            draw_dir_header(frame, header, view, &theme);
            draw_dir_body(frame, body, view, &theme);
            draw_dir_status(frame, status, view, &theme, &message);
        }
        None => {}
    }

    if app.help_visible {
        draw_help(
            frame,
            area,
            &theme,
            matches!(app.views.last(), Some(View::Dir(_))),
        );
    }
}

// ---------------------------------------------------------------------------------------
// Directory view
// ---------------------------------------------------------------------------------------

fn draw_dir_header(frame: &mut Frame<'_>, area: Rect, view: &DirView, theme: &Theme) {
    let [left, _, right] = pane_columns(area);
    render_path(frame, left, &view.roots[0], false, theme);
    render_path(frame, right, &view.roots[1], false, theme);
}

fn draw_dir_body(frame: &mut Frame<'_>, area: Rect, view: &mut DirView, theme: &Theme) {
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
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(format!(" {text}"), theme.status))),
            area,
        );
        return;
    }

    // Same three-column split as the file view: each side renders its own tree, aligned
    // row by row, with the marker column between. A name missing on one side leaves that
    // pane blank, so reading down one column tells you what that tree actually contains.
    let [left, marker, right] = pane_columns(area);
    draw_dir_pane(
        frame,
        left,
        view,
        &visible,
        top,
        cursor,
        SideIdx::LEFT,
        theme,
    );
    draw_dir_pane(
        frame,
        right,
        view,
        &visible,
        top,
        cursor,
        SideIdx::RIGHT,
        theme,
    );
    draw_dir_markers(frame, marker, view, &visible, top, cursor, theme);
}

/// One side of the directory tree.
///
/// Rows a side does not hold render as blank filler, which is what keeps the two panes
/// aligned: row `i` is the same entry in both, exactly as in the file view.
#[allow(clippy::too_many_arguments)]
fn draw_dir_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &DirView,
    visible: &[usize],
    top: usize,
    cursor: usize,
    side: SideIdx,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

    for (offset, &row_index) in visible
        .iter()
        .enumerate()
        .skip(top)
        .take(area.height as usize)
    {
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
        lines.push(Line::from(Span::styled(text, style)));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// The column between the panes. With colour off this carries the entire diff.
#[allow(clippy::too_many_arguments)]
fn draw_dir_markers(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &DirView,
    visible: &[usize],
    top: usize,
    cursor: usize,
    theme: &Theme,
) {
    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    for (offset, &row_index) in visible
        .iter()
        .enumerate()
        .skip(top)
        .take(area.height as usize)
    {
        let mut style = theme.marker;
        if offset == cursor {
            style = theme.cursor(style);
        }
        lines.push(Line::from(Span::styled(
            format!(" {} ", view.rows[row_index].status.glyph()),
            style,
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
        EntryStatus::TypeMismatch | EntryStatus::Unreadable(_) => theme.warning,
        EntryStatus::Pending => theme.filler,
    }
}

fn draw_dir_status(
    frame: &mut Frame<'_>,
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

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", parts.join(" \u{b7} ")),
            theme.status,
        ))),
        area,
    );
}

fn draw_file_header(frame: &mut Frame<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let [left, _, right] = pane_columns(area);
    let focus = view.focus;
    render_path(
        frame,
        left,
        view.cmp.left.path(),
        focus == SideIdx::LEFT,
        theme,
    );
    render_path(
        frame,
        right,
        view.cmp.right.path(),
        focus == SideIdx::RIGHT,
        theme,
    );
}

fn render_path(
    frame: &mut Frame<'_>,
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
    frame.render_widget(Paragraph::new(Line::from(Span::styled(text, style))), area);
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

fn draw_file_body(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &FileView,
    theme: &Theme,
    tab_width: u8,
) {
    let Some(alignment) = view.cmp.alignment.as_ref() else {
        draw_not_diffable(frame, area, view, theme);
        return;
    };
    let Some((left_text, right_text)) = view.cmp.texts() else {
        draw_not_diffable(frame, area, view, theme);
        return;
    };

    // A banner steals one row off the top when there is something the rows cannot say.
    let (banner, body) = match &view.cmp.note {
        Some(note) => {
            let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(area);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {note}"), theme.warning))),
                rows[0],
            );
            (Some(rows[0]), rows[1])
        }
        None => (None, area),
    };
    let _ = banner;

    let [left, marker, right] = pane_columns(body);

    let pane = |text: &TextFile, side: SideIdx, area: Rect, frame: &mut Frame<'_>| {
        frame.render_widget(
            DiffPane {
                alignment,
                text,
                side,
                top: view.top,
                cursor: view.cursor,
                h_scroll: view.h_scroll,
                gutter_width: gutter_width(text),
                tab_width,
                theme,
                focused: view.focus == side,
            },
            area,
        );
    };
    pane(left_text, SideIdx::LEFT, left, frame);
    pane(right_text, SideIdx::RIGHT, right, frame);

    draw_markers(frame, marker, view, theme);
}

/// The column between the panes: the only place the diff is legible with colour off.
fn draw_markers(frame: &mut Frame<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let Some(alignment) = view.cmp.alignment.as_ref() else {
        return;
    };
    let mut lines = Vec::with_capacity(area.height as usize);
    for dy in 0..area.height {
        let Some(row) = alignment.row(view.top + dy as usize) else {
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
        let style = if view.top + dy as usize == view.cursor {
            theme.cursor(theme.marker)
        } else {
            theme.marker
        };
        lines.push(Line::from(Span::styled(format!(" {glyph} "), style)));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_not_diffable(frame: &mut Frame<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let mut lines = vec![Line::from("")];
    if let Some(note) = &view.cmp.note {
        lines.push(Line::from(Span::styled(note.clone(), theme.warning)));
        lines.push(Line::from(""));
    }
    for (label, content) in [("left ", &view.cmp.left), ("right", &view.cmp.right)] {
        lines.push(Line::from(format!(
            "{label}  {}  {}",
            content.path().display(),
            describe(content)
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        if view.cmp.differ {
            "the two files differ"
        } else {
            "the two files are identical"
        },
        theme.status,
    )));

    let block = Block::bordered().title(" not comparable line by line ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn describe(content: &FileContent) -> String {
    match content {
        FileContent::Text(t) => format!("{} lines", t.line_count()),
        FileContent::Binary { size, .. } => format!("binary, {}", human_size(*size)),
        FileContent::TooLarge { size, .. } => format!("{}, too large", human_size(*size)),
        FileContent::Unreadable { message, .. } => message.clone(),
    }
}

fn draw_file_status(frame: &mut Frame<'_>, area: Rect, view: &FileView, theme: &Theme) {
    let rows = view.row_count();
    let mut parts = Vec::new();

    if rows > 0 {
        parts.push(format!("row {}/{}", view.cursor + 1, rows));
    }
    if let Some(alignment) = view.cmp.alignment.as_ref() {
        let line_at = |side: SideIdx| match alignment.cell(view.cursor, side) {
            Cell::Line(n) => n.display().to_string(),
            Cell::Filler => "-".to_string(),
        };
        parts.push(format!(
            "L:{} R:{}",
            line_at(SideIdx::LEFT),
            line_at(SideIdx::RIGHT)
        ));

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
    parts.push("? help".to_string());
    parts.push("q quit".to_string());

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", parts.join(" \u{b7} ")),
            theme.status,
        ))),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>, area: Rect, theme: &Theme, dir_mode: bool) {
    const FILE_HELP: &[(&str, &str)] = &[
        ("j k / arrows", "move down / up"),
        ("Ctrl-d Ctrl-u", "half page"),
        ("PgDn PgUp Space b", "page"),
        ("g G", "first / last row"),
        ("n p", "next / previous hunk"),
        ("h l / arrows", "scroll horizontally"),
        ("H L", "scroll horizontally by 8"),
        ("0", "back to column 0"),
        ("Tab", "switch focused side"),
        ("?", "toggle this help"),
        ("Esc", "back, or quit"),
        ("q  Ctrl-C", "quit"),
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
    let width = 48.min(area.width);
    let height = (help.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines: Vec<Line> = help
        .iter()
        .map(|(keys, what)| {
            Line::from(vec![
                Span::styled(format!(" {keys:<18}"), theme.header_focus),
                Span::styled((*what).to_string(), Style::new()),
            ])
        })
        .collect();

    Clear.render(popup, frame.buffer_mut());
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys ")),
        popup,
    );
}

/// Split an area into left pane, marker column, right pane.
fn pane_columns(area: Rect) -> [Rect; 3] {
    let cols = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(MARKER_WIDTH),
        Constraint::Fill(1),
    ])
    .split(area);
    [cols[0], cols[1], cols[2]]
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
        // Two columns cannot hold two panes plus a 3-wide marker; the layout must still
        // produce valid rects rather than panicking.
        let [left, marker, right] = pane_columns(Rect::new(0, 0, 2, 1));
        assert!(left.width + marker.width + right.width <= 2);
    }
}

#[cfg(test)]
mod render_tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, Options};
    use crate::diff::{DEFAULT_TIMEOUT, DiffOptions, compare_loaded};
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

    fn app_with(left: FileContent, right: FileContent) -> App {
        let opts = Options {
            diff: DiffOptions {
                timeout: DEFAULT_TIMEOUT,
                ..DiffOptions::default()
            },
            tab_width: 8,
            max_file_size: u64::MAX,
            follow_symlinks: false,
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
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| super::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
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
        // similar folds "old"->"new" and the added "extra" into a single replace op, so
        // this row is Changed with a filler on the left rather than a separate insert.
        assert_eq!(lines[3], "               \u{25ac}   3 extra");
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
            },
        );
        let lines = frame(&mut app, 46, 12);
        let joined = lines.join("\n");
        assert!(joined.contains("not comparable line by line"), "{joined}");
        assert!(joined.contains("right binary"), "{joined}");
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, DirFilter, Options};
    use crate::diff::DiffOptions;
    use crate::dirdiff::{DirEntryRow, EntryKind, EntryStatus, ScanEvent};
    use crate::model::SideIdx;
    use crate::theme::Theme;
    use crate::{Inputs, app};

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
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| super::draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use crate::app::{App, Options, View};
    use crate::diff::{DiffOptions, compare_loaded};
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
        app
    }

    fn render(app: &mut App, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| super::draw(f, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_backgrounds(buf: &Buffer, y: u16, width: u16) -> Vec<Option<ratatui::style::Color>> {
        (0..width)
            .map(|x| buf.cell((x, y)).unwrap().style().bg)
            .collect()
    }

    #[test]
    fn the_selected_row_is_highlighted_across_the_full_width() {
        let mut app = file_app("one\ntwo\nthree\n", "one\ntwo\nthree\n");
        let buf = render(&mut app, 40, 6);

        // Body starts at y=1. The cursor sits on the first row.
        let selected = row_backgrounds(&buf, 1, 40);
        let plain = row_backgrounds(&buf, 2, 40);

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
        let buf = render(&mut app, 40, 5);
        let selected = row_backgrounds(&buf, 1, 40);

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
        let first_row_bg = before.cell((2, 1)).unwrap().style().bg;

        app.active_file().unwrap().cursor = 1;
        let after = render(&mut app, 40, 6);

        assert_ne!(
            after.cell((2, 1)).unwrap().style().bg,
            first_row_bg,
            "row 1 should no longer be highlighted"
        );
        assert_eq!(
            after.cell((2, 2)).unwrap().style().bg,
            first_row_bg,
            "row 2 should now carry the highlight"
        );
    }

    #[test]
    fn a_changed_row_keeps_its_colour_when_selected() {
        // The reason the cursor lifts the background instead of reversing it: a selected
        // changed line must still look changed.
        let mut app = file_app("same\nold\n", "same\nnew\n");
        let mut buf = render(&mut app, 40, 5);
        let equal_selected = buf.cell((2, 1)).unwrap().style().bg;

        app.active_file().unwrap().cursor = 1;
        buf = render(&mut app, 40, 5);
        let changed_selected = buf.cell((2, 2)).unwrap().style().bg;

        assert_ne!(
            equal_selected, changed_selected,
            "a selected changed row must not look like a selected equal row"
        );
    }
}
