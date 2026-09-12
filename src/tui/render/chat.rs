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
    (app.input.lines().len() as u16).clamp(1, INPUT_MAX_ROWS) + 2
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(Span::styled(hint, app.styles.dimmed));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push(inner, HitTarget::ChatInput);

    let lines: Vec<Line> = app
        .input
        .lines()
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let prompt = if i == 0 { "> " } else { "  " };
            Line::from(vec![
                Span::styled(prompt, app.styles.accent),
                Span::styled(l.clone(), app.styles.normal),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);

    if focused && !app.core.slash_suggestions.is_empty() {
        draw_suggestions(frame, app, area);
    }

    if focused {
        let (row, col) = app.input.cursor_position();
        let x = inner.x + 2 + col as u16;
        let y = inner.y + row as u16;
        if x < inner.x + inner.width && y < inner.y + inner.height {
            frame.set_cursor_position((x, y));
        }
    }
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
