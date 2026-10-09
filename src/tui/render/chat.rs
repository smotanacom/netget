//! The chat input box at the foot of the stream.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::{DashboardApp, Focus};
use crate::tui::hit::HitTarget;

/// Input box grows with content, up to this many text rows.
const INPUT_MAX_ROWS: u16 = 5;

/// Rows the input box needs, borders included.
pub fn input_height(app: &DashboardApp) -> u16 {
    app.input.lines().len().clamp(1, INPUT_MAX_ROWS as usize) as u16 + 2
}

pub fn draw_input(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    let focused = app.focus == Focus::ChatInput;
    let border_style = if focused {
        app.styles.accent
    } else {
        app.styles.separator
    };
    let hint = if focused {
        if app.status.model.is_empty() {
            " no model · /commands only · Tab → cards "
        } else {
            " Enter send · Alt-Enter newline · Tab → cards "
        }
    } else {
        " Tab → chat "
    };
    let mut title = vec![];
    let replies: Vec<_> = app
        .snapshot
        .llm_activity
        .iter()
        .filter(|work| {
            matches!(
                work.source,
                crate::state::app_state::ConversationSource::User
            )
        })
        .collect();
    if let Some(first) = replies.first() {
        let extra = if replies.len() > 1 {
            format!(" · {} replies", replies.len())
        } else {
            String::new()
        };
        title.push(Span::styled(
            format!(
                " {} Generating reply · {}{extra} ",
                app.spinner(),
                crate::tui::metrics::human_duration(first.started_at.elapsed().as_secs())
            ),
            app.styles.reasoning,
        ));
    }
    title.push(Span::styled(hint, app.styles.dimmed));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push(inner, HitTarget::ChatInput);

    if inner.width <= 2 || inner.height == 0 {
        return;
    }
    let (row, col) = app.input.cursor_position();
    // InputState counts Unicode scalars; the terminal cursor counts display cells.
    let prefix: String = app.input.lines()[row].chars().take(col).collect();
    let cursor_column = Span::raw(prefix).width();
    let first_row = row.saturating_sub(inner.height as usize - 1);
    let first_column = cursor_column.saturating_sub(inner.width as usize - 3);

    let lines: Vec<Line> = app
        .input
        .lines()
        .iter()
        .enumerate()
        .skip(first_row)
        .take(inner.height as usize)
        .map(|(i, l)| {
            let prompt = if i == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(prompt, app.styles.accent),
                Span::styled(scroll_line(l, first_column), app.styles.normal),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    if focused && !app.core.slash_suggestions.is_empty() {
        draw_suggestions(frame, app, area);
    }

    if focused {
        let x = inner.x + 2 + (cursor_column - first_column) as u16;
        let y = inner.y + (row - first_row) as u16;
        frame.set_cursor_position((x, y));
    }
}

/// Clip whole graphemes by terminal columns, padding a partially clipped wide glyph.
/// This also keeps very long pasted lines independent of Paragraph's u16 scroll limit.
fn scroll_line(line: &str, columns: usize) -> String {
    let span = Span::raw(line);
    let mut skipped = 0;
    let mut out = String::new();
    for grapheme in span.styled_graphemes(ratatui::style::Style::default()) {
        let width = Span::raw(grapheme.symbol).width();
        if skipped < columns {
            skipped += width;
            if skipped > columns {
                out.push_str(&" ".repeat(skipped - columns));
            }
        } else {
            out.push_str(grapheme.symbol);
        }
    }
    out
}

fn draw_suggestions(frame: &mut Frame, app: &DashboardApp, input_area: Rect) {
    let count = app.core.slash_suggestions.len().min(8) as u16;
    if count == 0 || input_area.y == 0 {
        return;
    }
    let height = count + 2;
    let y = input_area.y.saturating_sub(height);
    let area = Rect {
        x: input_area.x,
        y,
        width: input_area.width,
        height: height.min(input_area.y),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(app.styles.separator)
        .title(Span::styled(" commands ", app.styles.dimmed));
    let inner = block.inner(area);
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(block, area);
    let lines: Vec<Line> = app
        .core
        .slash_suggestions
        .iter()
        .take(count as usize)
        .map(|s| Line::from(Span::styled(s.clone(), app.styles.info)))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}
