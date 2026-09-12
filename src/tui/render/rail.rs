//! The instance list.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{DashboardApp, Focus, Section, UiKey};
use crate::tui::hit::HitTarget;
use crate::tui::rail::{fit, list_rows, InstanceLine, ListRow, Tone};

use super::{pane_block, tone_style};

/// Sparkline width in the list row, when the pane is wide enough for one.
const SPARK_WIDTH: usize = 8;
/// Below this inner width the sparkline is dropped.
const SPARK_MIN_WIDTH: usize = 52;

pub fn draw(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    if area.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Instances;
    let servers = app.snapshot.servers.len();
    let clients = app.snapshot.clients.len();
    let title = Line::from(vec![
        Span::styled(" SERVERS ", app.styles.title),
        Span::styled(format!("{servers} "), app.styles.dimmed),
        Span::styled("· CLIENTS ", app.styles.title),
        Span::styled(format!("{clients} "), app.styles.dimmed),
    ]);
    let block = pane_block(app, focused)
        .title(title)
        .title_bottom(Span::styled(
            if focused {
                " ↑↓ pick · ←→ tab · Enter actions "
            } else {
                " Tab here "
            },
            app.styles.dimmed,
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let rows = list_rows(&app.snapshot);
    let cursor = cursor_index(app, &rows);

    // Scroll to keep the cursor visible.
    let viewport = inner.height as usize;
    let max_offset = rows.len().saturating_sub(viewport);
    let mut offset = app.instances.scroll.min(max_offset);
    if let Some(row) = cursor {
        if row < offset {
            offset = row;
        } else if row >= offset + viewport {
            offset = row + 1 - viewport;
        }
    }
    app.instances.scroll = offset;

    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(viewport);
    for (screen_index, row) in rows.iter().skip(offset).take(viewport).enumerate() {
        let absolute = offset + screen_index;
        let is_cursor = cursor == Some(absolute);
        lines.push(match row {
            ListRow::Header(section, count) => header_line(app, *section, *count, width),
            ListRow::New => new_line(app, is_cursor, focused),
            ListRow::Instance(key) => match crate::tui::rail::line_for(&app.snapshot, *key) {
                Some(line) => instance_line(app, &line, width, is_cursor, focused),
                None => Line::from(""),
            },
        });
        app.hits.push(
            Rect {
                x: inner.x,
                y: inner.y + screen_index as u16,
                width: inner.width,
                height: 1,
            },
            HitTarget::ListRow(absolute),
        );
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if rows.len() > viewport {
        let hint = format!(" {}–{}/{} ", offset + 1, offset + viewport, rows.len());
        let hint_width = (hint.chars().count() as u16).min(inner.width);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, app.styles.dimmed)),
            Rect {
                x: inner.x + inner.width.saturating_sub(hint_width),
                y: inner.y + inner.height - 1,
                width: hint_width,
                height: 1,
            },
        );
    }
}

/// Which list row the cursor is on, if any.
pub fn cursor_index(app: &DashboardApp, rows: &[ListRow]) -> Option<usize> {
    if app.instances.on_new {
        return rows.iter().position(|r| *r == ListRow::New);
    }
    let key = app.instances.selected?;
    rows.iter().position(|r| *r == ListRow::Instance(key))
}

fn header_line<'a>(app: &DashboardApp, section: Section, count: usize, width: usize) -> Line<'a> {
    let name = match section {
        Section::Servers => "servers",
        Section::Clients => "clients",
    };
    let label = format!(" {name} ");
    let _ = count; // the pane title carries the counts; the rule is just a divider
    let rule = "─".repeat(width.saturating_sub(label.chars().count()));
    Line::from(vec![
        Span::styled(label, app.styles.dimmed.add_modifier(Modifier::BOLD)),
        Span::styled(rule, app.styles.separator),
    ])
}

fn new_line<'a>(app: &DashboardApp, cursor: bool, focused: bool) -> Line<'a> {
    let style = row_style(app, app.styles.button, cursor, focused);
    Line::from(Span::styled("  + new server or client".to_string(), style))
}

/// Selection styling: inverted while the list has focus, accent-marked when
/// the cursor is elsewhere so the inspector's subject stays visible.
fn row_style(app: &DashboardApp, base: Style, cursor: bool, focused: bool) -> Style {
    if cursor && focused {
        app.styles.selected
    } else if cursor {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    }
}

/// `● #1 http     :8080        2⇄ ▁▂▅█▅▂▁▁ MANUAL`, fitted to `width`.
pub fn instance_line<'a>(
    app: &DashboardApp,
    line: &InstanceLine,
    width: usize,
    cursor: bool,
    focused: bool,
) -> Line<'a> {
    let marker = if cursor && !focused { "▸" } else { " " };
    let head = format!(
        "{marker}{} #{:<3}{:<8} ",
        line.glyph,
        line.id,
        fit(&line.protocol, 8)
    );
    let head_width = head.chars().count();

    // Right-hand side: peers, sparkline, waiting chip, driver badge.
    let mut right: Vec<(String, Tone)> = Vec::new();
    if let Some(error) = &line.error {
        // An error row spends its width on the reason.
        let target = format!("{} ", line.target);
        let room = width.saturating_sub(head_width + target.chars().count());
        let mut spans = vec![
            Span::styled(
                head,
                row_style(app, tone_style(app, line.glyph_tone), cursor, focused),
            ),
            Span::styled(target, row_style(app, app.styles.normal, cursor, focused)),
            Span::styled(
                fit(error, room),
                row_style(app, app.styles.error, cursor, focused),
            ),
        ];
        pad_line(
            &mut spans,
            width,
            row_style(app, app.styles.normal, cursor, focused),
        );
        return Line::from(spans);
    }
    if !line.peers.is_empty() {
        right.push((format!("{:>3}", line.peers), Tone::Dim));
    }
    if width >= SPARK_MIN_WIDTH {
        let spark = app
            .instances
            .metrics
            .get(&line.key)
            .map(|m| m.sparkline(SPARK_WIDTH))
            .unwrap_or_else(|| " ".repeat(SPARK_WIDTH));
        right.push((format!(" {spark}"), Tone::Accent));
    }
    if line.waiting > 0 {
        right.push((format!(" ⚠{}", line.waiting), Tone::Bad));
    }
    right.push((format!(" {:>6}", line.driver.label()), line.driver.tone()));
    let right_width: usize = right.iter().map(|(s, _)| s.chars().count()).sum();

    let room = width.saturating_sub(head_width + right_width + 1);
    let target = fit(&line.target, room);
    let target_padded = format!("{target:<room$} ");

    let mut spans = vec![
        Span::styled(
            head,
            row_style(app, tone_style(app, line.glyph_tone), cursor, focused),
        ),
        Span::styled(
            target_padded,
            row_style(app, app.styles.normal, cursor, focused),
        ),
    ];
    for (text, tone) in right {
        spans.push(Span::styled(
            text,
            row_style(app, tone_style(app, tone), cursor, focused),
        ));
    }
    pad_line(
        &mut spans,
        width,
        row_style(app, app.styles.normal, cursor, focused),
    );
    Line::from(spans)
}

/// Pad a line to the full width so a reversed selection spans the pane.
fn pad_line(spans: &mut Vec<Span<'_>>, width: usize, style: Style) {
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

/// Whether `key` is drawn as a server or a client, for callers that colour
/// by kind.
pub fn kind_tone(key: UiKey) -> Tone {
    match key {
        UiKey::Server(_) => Tone::Server,
        UiKey::Client(_) => Tone::Client,
    }
}
