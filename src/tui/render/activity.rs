//! The activity feed pane.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::activity::{ActivityEntry, ActivityFeed, ActivityKind};
use crate::tui::app::{DashboardApp, Focus, UiKey};
use crate::tui::chat::ScrollPos;
use crate::tui::hit::HitTarget;
use crate::tui::rail::fit;

use super::pane_block;

/// The entries the pane shows, under the current filters.
pub fn visible_entries(app: &DashboardApp) -> Vec<&ActivityEntry> {
    let level = app.core.log_level;
    let only = if app.activity.only_selected {
        app.selected()
    } else {
        None
    };
    app.activity
        .entries
        .iter()
        .filter(|e| ActivityFeed::passes(e, level, only))
        .collect()
}

pub fn draw(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    if area.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Activity;
    let mut title = vec![Span::styled(" ACTIVITY ", app.styles.title)];
    if app.activity.only_selected {
        if let Some(instance) = app.selected_instance() {
            title.push(Span::styled(
                format!("· only {} ", instance.tag()),
                app.styles.warning,
            ));
        }
    }
    title.push(Span::styled(
        format!("· log:{} ", app.core.log_level.as_str().to_lowercase()),
        app.styles.dimmed,
    ));
    let block = pane_block(app, focused).title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push(inner, HitTarget::Activity);
    if inner.height == 0 {
        return;
    }

    let entries = visible_entries(app);
    if entries.is_empty() {
        let hint = if app.activity.entries.is_empty() {
            "Nothing has happened yet. Instances starting, peers connecting, every \
             request and its answer, and anything waiting on you all land here."
        } else {
            "(nothing matches the current filter)"
        };
        frame.render_widget(
            Paragraph::new(hint)
                .style(app.styles.dimmed)
                .wrap(ratatui::widgets::Wrap { trim: true }),
            Rect {
                x: inner.x + 1,
                y: inner.y + 1.min(inner.height.saturating_sub(1)),
                width: inner.width.saturating_sub(2),
                height: inner.height.saturating_sub(1).max(1),
            },
        );
        return;
    }
    let total = entries.len();
    let viewport = inner.height as usize;
    let max_offset = total.saturating_sub(viewport);
    let mut offset = match app.activity.scroll {
        ScrollPos::Follow => max_offset,
        ScrollPos::Up(up) => max_offset.saturating_sub(up),
    };
    // A cursor (while focused) pins the window around itself.
    let cursor = if focused { app.activity.cursor } else { None };
    if let Some(cursor) = cursor {
        if cursor < offset {
            offset = cursor;
        } else if cursor >= offset + viewport {
            offset = cursor + 1 - viewport;
        }
    }

    let width = inner.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(viewport);
    // Anchor to the bottom while the feed is short.
    let shown = entries.len().saturating_sub(offset).min(viewport);
    for _ in shown..viewport {
        lines.push(Line::from(""));
    }
    let pad = viewport - shown;
    let mut hits: Vec<(Rect, HitTarget)> = Vec::with_capacity(shown);
    for (screen_index, entry) in entries.iter().skip(offset).take(viewport).enumerate() {
        let absolute = offset + screen_index;
        let is_cursor = cursor == Some(absolute);
        lines.push(entry_line(app, entry, width, is_cursor));
        hits.push((
            Rect {
                x: inner.x,
                y: inner.y + (pad + screen_index) as u16,
                width: inner.width,
                height: 1,
            },
            HitTarget::ActivityRow(absolute),
        ));
    }
    drop(entries);
    for (rect, target) in hits {
        app.hits.push(rect, target);
    }
    frame.render_widget(Paragraph::new(lines), inner);

    if app.activity.unseen > 0 {
        let label = format!(" {} new ↓ ", app.activity.unseen);
        let label_width = (label.chars().count() as u16).min(inner.width);
        frame.render_widget(
            Paragraph::new(label).style(app.styles.selected),
            Rect {
                x: inner.x + inner.width.saturating_sub(label_width),
                y: inner.y + inner.height.saturating_sub(1),
                width: label_width,
                height: 1,
            },
        );
    }
}

fn kind_glyph_and_style(app: &DashboardApp, kind: ActivityKind) -> (&'static str, Style) {
    use crate::ui::app::LogLevel;
    match kind {
        ActivityKind::Log(LogLevel::Error) => ("✗ ", app.styles.error),
        ActivityKind::Log(LogLevel::Warn) => ("⚠ ", app.styles.warning),
        ActivityKind::Log(LogLevel::Info) => ("· ", app.styles.info),
        ActivityKind::Log(LogLevel::Debug) => ("○ ", app.styles.debug),
        ActivityKind::Log(LogLevel::Trace) => ("· ", app.styles.trace),
        ActivityKind::Lifecycle => ("◆ ", app.styles.success),
        ActivityKind::Peer => ("⇄ ", app.styles.connection),
        ActivityKind::Request => ("→ ", app.styles.normal),
        ActivityKind::Waiting => ("⚠ ", app.styles.error),
        ActivityKind::Failure => ("✗ ", app.styles.error),
    }
}

fn entry_line<'a>(
    app: &DashboardApp,
    entry: &ActivityEntry,
    width: usize,
    cursor: bool,
) -> Line<'a> {
    let (glyph, style) = kind_glyph_and_style(app, entry.event.kind);
    let tag_style = match entry.event.owner {
        Some(UiKey::Server(_)) => app.styles.server,
        Some(UiKey::Client(_)) => app.styles.client,
        None => app.styles.dimmed,
    };
    let time = format!("{} ", entry.time);
    let tag = if entry.event.tag.is_empty() {
        String::new()
    } else {
        format!("{:<9}", fit(&entry.event.tag, 9))
    };
    let used = time.chars().count() + tag.chars().count() + glyph.chars().count();
    let text = fit(&entry.event.text, width.saturating_sub(used));
    let sel = |s: Style| if cursor { app.styles.selected } else { s };
    let mut spans = vec![
        Span::styled(time, sel(app.styles.dimmed)),
        Span::styled(tag, sel(tag_style)),
        Span::styled(glyph, sel(style)),
        Span::styled(text.clone(), sel(style)),
    ];
    let filled = used + text.chars().count();
    if cursor && filled < width {
        spans.push(Span::styled(
            " ".repeat(width - filled),
            app.styles.selected,
        ));
    }
    Line::from(spans)
}
