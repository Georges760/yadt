//! Layout and rendering. Free functions taking a `Frame`, per the house pattern.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};

use crate::app::{App, FileView, View};
use crate::diff::human_size;
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

    match app.views.last_mut() {
        Some(View::File(view)) => {
            view.clamp(body);
            draw_file_header(frame, header, view, &theme);
            draw_file_body(frame, body, view, &theme, tab_width);
            draw_file_status(frame, status, view, &theme);
        }
        None => {}
    }

    if app.help_visible {
        draw_help(frame, area, &theme);
    }
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
        lines.push(Line::from(Span::styled(format!(" {glyph} "), theme.marker)));
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

fn draw_help(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    const HELP: &[(&str, &str)] = &[
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

    let width = 44.min(area.width);
    let height = (HELP.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let lines: Vec<Line> = HELP
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
