//! Frame composition.
//!
//! Left column: the instance canvas. Right column: the stream, then the
//! input box. One status line along the bottom; modals on top of everything.

pub mod cards;
pub mod chat;
pub mod overlay;
pub mod rail;
pub mod status_bar;
pub mod stream;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::app::DashboardApp;
use crate::tui::rail::Tone;

/// Minimum terminal size the dashboard renders at.
pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;

/// The management column: never narrower than this, never wider than what
/// leaves the feed readable.
const LEFT_MIN: u16 = 38;
const LEFT_MAX: u16 = 76;
const LEFT_PERCENT: u16 = 48;
const RIGHT_MIN: u16 = 36;

pub fn draw(frame: &mut Frame, app: &mut DashboardApp) {
    app.hits.clear();
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let notice = Paragraph::new(format!(
            "Terminal too small: {}x{} (need {}x{}).\nResize, or run with --legacy-tui.",
            area.width, area.height, MIN_WIDTH, MIN_HEIGHT
        ))
        .style(app.styles.warning);
        frame.render_widget(notice, area);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    let left_width = ((body.width as u32 * LEFT_PERCENT as u32) / 100) as u16;
    let left_width = left_width
        .clamp(LEFT_MIN, LEFT_MAX)
        .min(body.width.saturating_sub(RIGHT_MIN));
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_width), Constraint::Min(RIGHT_MIN)])
        .split(body);
    let left = columns[0];
    let right = columns[1];

    cards::draw(frame, app, left);

    let input_height = chat::input_height(app);
    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(input_height)])
        .split(right);
    stream::draw(frame, app, right_rows[0]);
    chat::draw_input(frame, app, right_rows[1]);

    status_bar::draw(frame, app, status);

    if app.modal().is_some() {
        overlay::draw(frame, app, area);
    }
}

/// A pane frame: rounded, accent-coloured while focused.
pub fn pane_block<'a>(app: &DashboardApp, focused: bool) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            app.styles.accent
        } else {
            app.styles.separator
        })
}

/// Resolve a semantic tone against the palette.
pub fn tone_style(app: &DashboardApp, tone: Tone) -> Style {
    match tone {
        Tone::Normal => app.styles.normal,
        Tone::Dim => app.styles.dimmed,
        Tone::Good => app.styles.success,
        Tone::Warn => app.styles.warning,
        Tone::Bad => app.styles.error,
        Tone::Accent => app.styles.info,
        Tone::Server => app.styles.server,
        Tone::Client => app.styles.client,
        Tone::Title => app.styles.title,
        Tone::Reasoning => app.styles.reasoning,
    }
}

/// Centre a modal rect of the given percentage inside `area`.
pub fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    centered_capped(area, percent_x, percent_y, u16::MAX, u16::MAX)
}

/// Centre a box sized by percentage but capped at what its content needs.
pub fn centered_capped(
    area: Rect,
    percent_x: u16,
    percent_y: u16,
    max_cols: u16,
    max_rows: u16,
) -> Rect {
    let width = (area.width * percent_x / 100)
        .min(max_cols)
        .max(24)
        .min(area.width);
    let height = (area.height * percent_y / 100)
        .min(max_rows)
        .max(5)
        .min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}
