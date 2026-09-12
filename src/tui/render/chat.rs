//! Chat pane: the conversation above, the input box below.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{DashboardApp, Focus};
use crate::tui::chat::{ChatState, EntryKind, ScrollPos};
use crate::tui::hit::HitTarget;

use super::pane_block;

/// Input box grows with content, up to this many text rows.
const INPUT_MAX_ROWS: u16 = 5;

/// Rows the input box needs, borders included.
pub fn input_height(app: &DashboardApp) -> u16 {
    (app.input.lines().len() as u16).clamp(1, INPUT_MAX_ROWS) + 2
}

fn glyph_and_style(app: &DashboardApp, kind: EntryKind) -> (&'static str, Style) {
    match kind {
        EntryKind::User => ("▶ ", app.styles.user),
        EntryKind::Reasoning => ("∴ ", app.styles.reasoning),
        EntryKind::System => ("  ", app.styles.normal),
        EntryKind::Log(level) => {
            use crate::ui::app::LogLevel;
            let glyph = match level {
                LogLevel::Error => "✗ ",
                LogLevel::Warn => "⚠ ",
                LogLevel::Info => "● ",
                LogLevel::Debug => "○ ",
                LogLevel::Trace => "· ",
            };
            (glyph, app.styles.for_log_level(level))
        }
    }
}

fn history_lines<'a>(app: &DashboardApp) -> Vec<Line<'a>> {
    let level = app.core.log_level;
    let mut lines: Vec<Line> = Vec::new();
    for entry in app
        .chat
        .entries
        .iter()
        .filter(|e| ChatState::passes_filter(e, level))
    {
        let (glyph, style) = glyph_and_style(app, entry.kind);
        for (i, text_line) in entry.text.split('\n').enumerate() {
            let prefix = if i == 0 { glyph } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(text_line.to_string(), style),
            ]));
        }
    }
    lines
}

/// Rendered rows the history would take at `width`, so the layout can size
/// the pane to its conversation. Zero when there is nothing to show.
pub fn history_height_needed(app: &DashboardApp, width: u16) -> u16 {
    let wrap_width = width.max(1) as usize;
    history_lines(app)
        .iter()
        .map(|line| {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            w.div_ceil(wrap_width).max(1)
        })
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

pub fn draw_history(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    if area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::ChatHistory;
    let block = pane_block(app, focused).title(Span::styled(" CHAT ", app.styles.title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push(inner, HitTarget::ChatHistory);
    if inner.height == 0 {
        return;
    }

    let mut lines = history_lines(app);

    // Anchor to the bottom: a conversation grows upward from the input box.
    let viewport = inner.height as usize;
    let wrap_width = inner.width.max(1) as usize;
    let rendered_height: usize = lines
        .iter()
        .map(|line| {
            let width: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            width.div_ceil(wrap_width).max(1)
        })
        .sum();
    if rendered_height < viewport {
        let padding = viewport - rendered_height;
        let mut padded = vec![Line::from(""); padding];
        padded.extend(lines);
        lines = padded;
    }

    // ratatui's Paragraph scroll counts *rendered* lines, and we wrap, so
    // approximate by scrolling in unwrapped lines — accurate for typical
    // one-line entries and monotonic for longer ones.
    let total = lines.len() as u16;
    let viewport = inner.height;
    let max_offset = total.saturating_sub(viewport);
    let offset = match app.chat.scroll {
        ScrollPos::Follow => max_offset,
        ScrollPos::Up(up) => max_offset.saturating_sub(up as u16),
    };

    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    frame.render_widget(paragraph, inner);

    if app.chat.unseen > 0 {
        let label = format!(" {} new ↓ ", app.chat.unseen);
        let width = (label.chars().count() as u16).min(inner.width);
        let pill = Rect {
            x: inner.x + inner.width.saturating_sub(width),
            y: inner.y + inner.height.saturating_sub(1),
            width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(label).style(app.styles.selected), pill);
    }
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
            " no model · /commands only · Tab → panes "
        } else {
            " Enter send · Alt-Enter newline · Tab → panes "
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
