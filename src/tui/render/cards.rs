//! The instance canvas pane.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{DashboardApp, Focus};
use crate::tui::cards::{self, Row};
use crate::tui::hit::HitTarget;
use crate::tui::rail::fit;

use super::{pane_block, tone_style};

pub fn draw(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    if area.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Cards;
    let servers = app.snapshot.servers.len();
    let clients = app.snapshot.clients.len();
    let title = Line::from(vec![
        Span::styled(" SERVERS ", app.styles.title),
        Span::styled(format!("{servers} "), app.styles.dimmed),
        Span::styled("· CLIENTS ", app.styles.title),
        Span::styled(format!("{clients} "), app.styles.dimmed),
    ]);
    let hint = if focused {
        " ↑↓ rows · ←→ buttons · Enter · a new "
    } else {
        " Tab here "
    };
    let block = pane_block(app, focused)
        .title(title)
        .title_bottom(Span::styled(hint, app.styles.dimmed));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    app.cards.width = inner.width as usize;

    let rows = app.rows();
    app.clamp_cursor_to(&rows);

    // Scroll to keep the cursor's row visible.
    let viewport = inner.height as usize;
    let max_offset = rows.len().saturating_sub(viewport);
    let mut offset = app.cards.scroll.min(max_offset);
    let cursor = app.cards.row;
    if cursor < offset {
        offset = cursor;
    } else if cursor >= offset + viewport {
        offset = cursor + 1 - viewport;
    }
    app.cards.scroll = offset;

    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(viewport);
    let mut hits: Vec<(Rect, HitTarget)> = Vec::new();
    for (screen_index, row) in rows.iter().skip(offset).take(viewport).enumerate() {
        let absolute = offset + screen_index;
        let is_cursor = focused && absolute == cursor;
        let y = inner.y + screen_index as u16;
        let (line, spans_width, button_spans) = row_line(app, row, width, is_cursor, app.cards.col);
        lines.push(line);

        // The label region, then each button, for the mouse.
        if row.has_label_stop() || row.header.is_some() {
            hits.push((
                Rect {
                    x: inner.x,
                    y,
                    width: (spans_width as u16).max(1).min(inner.width),
                    height: 1,
                },
                HitTarget::CardRow {
                    row: absolute,
                    col: 0,
                },
            ));
        }
        let offset_col = usize::from(row.has_label_stop());
        for (i, (x, w)) in button_spans.iter().enumerate() {
            hits.push((
                Rect {
                    x: inner.x + *x as u16,
                    y,
                    width: *w as u16,
                    height: 1,
                },
                HitTarget::CardRow {
                    row: absolute,
                    col: offset_col + i,
                },
            ));
        }
    }
    for (rect, target) in hits {
        app.hits.push(rect, target);
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if rows.len() > viewport {
        let hint = format!(
            " {}–{}/{} ",
            offset + 1,
            (offset + viewport).min(rows.len()),
            rows.len()
        );
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

/// Render one row. Returns the line, the columns its label part used, and
/// each button's `(x, width)` for hit-testing.
fn row_line<'a>(
    app: &DashboardApp,
    row: &Row,
    width: usize,
    is_cursor: bool,
    col: usize,
) -> (Line<'a>, usize, Vec<(usize, usize)>) {
    // A header row is the instance summary line; its cursor is the whole row.
    if let Some(line) = &row.header {
        let selected = is_cursor && col == 0;
        let rendered =
            super::rail::instance_line(app, line, width, selected, app.focus == Focus::Cards);
        return (rendered, width, Vec::new());
    }

    let label_selected = is_cursor && col == 0 && row.has_label_stop();
    let indent = " ".repeat(row.depth as usize * 2);
    let marker = match row.expanded {
        Some(true) => "▾ ",
        Some(false) => "▸ ",
        None => "  ",
    };
    let mut spans: Vec<Span> = vec![Span::styled(
        format!("{indent}{marker}"),
        if label_selected {
            app.styles.selected
        } else {
            app.styles.separator
        },
    )];
    let mut used = indent.chars().count() + 2;

    // Buttons first, so the label yields to them rather than the reverse.
    let button_texts: Vec<String> = row
        .buttons
        .iter()
        .map(|b| {
            let inner = if row.button_width > 0 {
                format!("{:<w$}", b.label, w = row.button_width.saturating_sub(4))
            } else {
                b.label.clone()
            };
            format!("[ {inner} ]")
        })
        .collect();
    let buttons_width: usize = button_texts.iter().map(|t| t.chars().count() + 1).sum();

    let label_room = width.saturating_sub(used + buttons_width);
    let mut label_used = 0usize;
    for (text, tone) in &row.spans {
        let room = label_room.saturating_sub(label_used);
        if room == 0 {
            break;
        }
        let shown = fit(text, room);
        label_used += shown.chars().count();
        spans.push(Span::styled(
            shown,
            if label_selected {
                app.styles.selected
            } else {
                tone_style(app, *tone)
            },
        ));
    }
    used += label_used;
    let label_width = used;

    let mut button_spans = Vec::new();
    let offset_col = usize::from(row.has_label_stop());
    for (i, (button, text)) in row.buttons.iter().zip(button_texts.iter()).enumerate() {
        if !row.spans.is_empty() || i > 0 {
            spans.push(Span::raw(" "));
            used += 1;
        }
        let selected = is_cursor && col == offset_col + i;
        let style: Style = if selected {
            app.styles.selected
        } else if !button.enabled {
            app.styles.dimmed
        } else {
            app.styles.button
        };
        let w = text.chars().count();
        button_spans.push((used, w));
        spans.push(Span::styled(text.clone(), style));
        used += w;
    }
    if label_selected && used < width {
        spans.push(Span::styled(" ".repeat(width - used), app.styles.selected));
    }
    let _ = Modifier::BOLD;
    (Line::from(spans), label_width, button_spans)
}
