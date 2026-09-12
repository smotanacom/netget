//! The one-line instance summary a card's header row renders.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::app::{DashboardApp, UiKey};
use crate::tui::rail::{fit, InstanceLine, Tone};

use super::tone_style;

/// Sparkline width in the list row, when the pane is wide enough for one.
const SPARK_WIDTH: usize = 8;
/// Below this inner width the sparkline is dropped.
const SPARK_MIN_WIDTH: usize = 52;

/// Selection styling: inverted while the list has focus, accent-marked when
/// the column is not focused so the cursor's card stays visible.
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
            .cards
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
