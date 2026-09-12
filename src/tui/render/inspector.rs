//! The inspector pane: tab strip, action bar, body.

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::app::{DashboardApp, Focus};
use crate::tui::hit::HitTarget;
use crate::tui::inspector::{self, InspectorView};

use super::{pane_block, tone_style};

pub fn draw(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    if area.height < 3 {
        return;
    }
    let focused = app.focus == Focus::Inspector;

    let Some(instance) = app.selected_instance() else {
        let block =
            pane_block(app, focused).title(Span::styled(" nothing selected ", app.styles.dimmed));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        draw_welcome(frame, app, inner);
        return;
    };
    let key = instance.key();
    let view = inspector::build(
        instance,
        &app.inspector,
        app.instances.metrics.get(&key),
        area.width.saturating_sub(2) as usize,
    );

    // Clamp the item cursor to what this tab has.
    let item_count = view.item_count();
    if item_count == 0 {
        app.inspector.item = 0;
    } else if app.inspector.item >= item_count {
        app.inspector.item = item_count - 1;
    }

    let title = Line::from(vec![Span::styled(
        format!(" {} ", view.title),
        app.styles.title,
    )]);
    let hint = if focused {
        " ↑↓ items · ←→ tab · Enter open · Space actions "
    } else {
        " Space actions "
    };
    let block = pane_block(app, focused)
        .title(title)
        .title_bottom(Span::styled(hint, app.styles.dimmed));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.push(inner, HitTarget::InspectorBody);
    if inner.height == 0 {
        return;
    }

    draw_tabs(
        frame,
        app,
        &view,
        Rect {
            y: inner.y,
            height: 1,
            ..inner
        },
    );
    let body = Rect {
        x: inner.x,
        y: inner.y + 1,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    if body.height == 0 {
        return;
    }
    draw_body(frame, app, &view, body, focused);
}

fn draw_welcome(frame: &mut Frame, app: &DashboardApp, area: Rect) {
    let key = |k: &'static str, what: &'static str| {
        Line::from(vec![
            Span::styled(format!("  {k:<6}"), app.styles.accent),
            Span::styled(what, app.styles.dimmed),
        ])
    };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled("  Nothing selected yet.", app.styles.normal)),
        Line::from(""),
        key("a", "start a server or a client"),
        key("↑ ↓", "pick one; ↓ walks into it"),
        key("Enter", "everything you can do to it"),
        key("Tab", "to the feed and the chat"),
        key("F1", "every key"),
        Line::from(""),
        Line::from(Span::styled(
            "  Or ask the model in chat — “start an http server on 8080”.",
            app.styles.dimmed,
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_tabs(frame: &mut Frame, app: &mut DashboardApp, view: &InspectorView, area: Rect) {
    let width = area.width as usize;
    let labels: Vec<String> = view
        .tabs
        .iter()
        .map(|t| format!(" {} ", t.label(view.key)))
        .collect();
    let selected = view.tabs.iter().position(|t| *t == view.tab).unwrap_or(0);
    let widths: Vec<usize> = labels.iter().map(|l| l.chars().count()).collect();

    // Separators when there is room for them; the labels alone otherwise.
    let with_separators: usize = widths.iter().sum::<usize>() + widths.len().saturating_sub(1);
    let separator = if with_separators <= width {
        Some("│")
    } else {
        None
    };
    let total = if separator.is_some() {
        with_separators
    } else {
        widths.iter().sum::<usize>()
    };

    // When even that does not fit, show a window of tabs around the selected
    // one, with ‹ › marking what is off-screen. Six tabs at 36 columns is the
    // client at the minimum terminal size.
    let mut start = 0usize;
    if total > width {
        let room = width.saturating_sub(2);
        while start < selected && widths[start..=selected].iter().sum::<usize>() > room {
            start += 1;
        }
    }

    let mut spans: Vec<Span> = Vec::new();
    let mut x = 0u16;
    if start > 0 {
        spans.push(Span::styled("‹", app.styles.dimmed));
        x += 1;
    }
    let mut cut = false;
    for (index, (tab, label)) in view.tabs.iter().zip(labels.iter()).enumerate().skip(start) {
        let label_width = widths[index] as u16;
        let trailing = if index + 1 < view.tabs.len() { 1 } else { 0 };
        if x + label_width + trailing > area.width {
            cut = true;
            break;
        }
        let style = if *tab == view.tab {
            app.styles.accent.add_modifier(Modifier::UNDERLINED)
        } else {
            app.styles.dimmed
        };
        spans.push(Span::styled(label.clone(), style));
        app.hits.push(
            Rect {
                x: area.x + x,
                y: area.y,
                width: label_width,
                height: 1,
            },
            HitTarget::InspectorTab(*tab),
        );
        x += label_width;
        if let Some(sep) = separator {
            if index + 1 < view.tabs.len() {
                spans.push(Span::styled(sep, app.styles.separator));
                x += 1;
            }
        }
    }
    if cut {
        spans.push(Span::styled("›", app.styles.dimmed));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_body(
    frame: &mut Frame,
    app: &mut DashboardApp,
    view: &InspectorView,
    area: Rect,
    focused: bool,
) {
    let item_lines = view.item_lines();
    let selected_line = item_lines.get(app.inspector.item).copied();

    // Scroll so the selected item stays visible.
    let viewport = area.height as usize;
    let total = view.lines.len();
    let max_offset = total.saturating_sub(viewport);
    let mut offset = app.inspector.scroll.min(max_offset);
    if let Some(line) = selected_line {
        if line < offset {
            offset = line;
        } else if line >= offset + viewport {
            offset = line + 1 - viewport;
        }
    }
    app.inspector.scroll = offset;

    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(viewport);
    let mut item_ordinal = item_lines.partition_point(|l| *l < offset);
    for (screen_index, line) in view.lines.iter().skip(offset).take(viewport).enumerate() {
        let absolute = offset + screen_index;
        let is_item = line.item.is_some();
        let is_selected = is_item && selected_line == Some(absolute);
        let gutter = if is_item && selected_line == Some(absolute) {
            "▸ "
        } else {
            "  "
        };
        let mut spans = vec![Span::styled(
            gutter,
            if is_selected && focused {
                app.styles.selected
            } else {
                app.styles.accent
            },
        )];
        let mut used = 2usize;
        for (text, tone) in &line.spans {
            let room = width.saturating_sub(used);
            if room == 0 {
                break;
            }
            let shown = crate::tui::rail::fit(text, room);
            used += shown.chars().count();
            let style = if is_selected && focused {
                app.styles.selected
            } else {
                tone_style(app, *tone)
            };
            spans.push(Span::styled(shown, style));
        }
        if is_selected && focused && used < width {
            spans.push(Span::styled(" ".repeat(width - used), app.styles.selected));
        }
        lines.push(Line::from(spans));

        if is_item {
            app.hits.push(
                Rect {
                    x: area.x,
                    y: area.y + screen_index as u16,
                    width: area.width,
                    height: 1,
                },
                HitTarget::InspectorItem(item_ordinal),
            );
            item_ordinal += 1;
        }
    }
    frame.render_widget(Paragraph::new(lines), area);

    if total > viewport {
        let hint = format!(
            " {}–{}/{} ",
            offset + 1,
            (offset + viewport).min(total),
            total
        );
        let hint_width = (hint.chars().count() as u16).min(area.width);
        frame.render_widget(
            Paragraph::new(Span::styled(hint, app.styles.dimmed)),
            Rect {
                x: area.x + area.width.saturating_sub(hint_width),
                y: area.y + area.height - 1,
                width: hint_width,
                height: 1,
            },
        );
    }
}
