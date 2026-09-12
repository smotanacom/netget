//! Bottom status line: what is running, what is waiting on you, which model
//! (if any) is behind the LLM paths, and the toggles. Every segment is
//! clickable.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::DashboardApp;
use crate::tui::hit::{HitTarget, SegmentId};

pub fn draw(frame: &mut Frame, app: &mut DashboardApp, area: Rect) {
    let servers = app.snapshot.servers.len();
    let clients = app.snapshot.clients.len();
    let waiting = app.waiting_count();

    // (text, id, style)
    let mut segments: Vec<(String, SegmentId, ratatui::style::Style)> = Vec::new();
    segments.push((
        format!(
            " {servers} server{} · {clients} client{} ",
            if servers == 1 { "" } else { "s" },
            if clients == 1 { "" } else { "s" }
        ),
        SegmentId::Instances,
        app.styles.dimmed,
    ));
    if waiting > 0 {
        segments.push((
            format!(" ⚠ {waiting} waiting for you "),
            SegmentId::Waiting,
            app.styles.error,
        ));
    }
    segments.push((
        if app.status.model.is_empty() {
            " llm: none ".to_string()
        } else {
            format!(" llm: {} ", app.status.model)
        },
        SegmentId::Model,
        app.styles.dimmed,
    ));
    if app.status.llm_calls > 0 {
        segments.push((
            format!(
                " {} call{} · {}k/{}k tok ",
                app.status.llm_calls,
                if app.status.llm_calls == 1 { "" } else { "s" },
                app.status.input_tokens / 1000,
                app.status.output_tokens / 1000
            ),
            SegmentId::Usage,
            app.styles.dimmed,
        ));
    }
    if app.status.active_conversations > 0 {
        segments.push((
            format!(" ∴ {} thinking ", app.status.active_conversations),
            SegmentId::Usage,
            app.styles.reasoning,
        ));
    }
    segments.push((
        format!(" log:{} ", app.core.log_level.as_str()),
        SegmentId::LogLevel,
        app.styles.dimmed,
    ));
    segments.push((
        format!(" web:{} ", app.status.web_search),
        SegmentId::WebSearch,
        app.styles.dimmed,
    ));
    segments.push((
        format!(" handler:{} ", app.status.handler_mode),
        SegmentId::Handler,
        app.styles.dimmed,
    ));
    if let Some(notice) = &app.status.notice {
        segments.push((format!(" {notice} "), SegmentId::Usage, app.styles.warning));
    }

    // `F1 keys` is pinned to the right edge whatever else fits: the way to
    // discover everything must not be the first thing a busy bar drops.
    let help = " F1 keys ";
    let help_width = help.chars().count() as u16;
    let room = area.width.saturating_sub(help_width + 1);

    let mut spans: Vec<Span> = Vec::new();
    let mut x = area.x;
    for (index, (text, id, style)) in segments.iter().enumerate() {
        let width = text.chars().count() as u16;
        let needed = width + if index > 0 { 1 } else { 0 };
        if x + needed > area.x + room {
            break;
        }
        if index > 0 {
            spans.push(Span::styled("│", app.styles.separator));
            x += 1;
        }
        spans.push(Span::styled(text.clone(), *style));
        app.hits.push(
            Rect {
                x,
                y: area.y,
                width,
                height: 1,
            },
            HitTarget::StatusSegment(*id),
        );
        x += width;
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    if area.width > help_width {
        let help_area = Rect {
            x: area.x + area.width - help_width,
            y: area.y,
            width: help_width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("│", app.styles.separator),
                Span::styled(help, app.styles.dimmed),
            ])),
            Rect {
                x: help_area.x - 1,
                width: help_width + 1,
                ..help_area
            },
        );
        app.hits
            .push(help_area, HitTarget::StatusSegment(SegmentId::Help));
    }
}
