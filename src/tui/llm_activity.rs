//! Shared activity labels and animation for the cards, chat and footer.

use crate::state::app_state::ConversationSource;
use crate::state::llm_activity::LlmActivity;
use crate::tui::app::UiKey;
use crate::tui::metrics::human_duration;

pub fn spinner(tick: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[tick % FRAMES.len()]
}

pub fn owner(activity: &LlmActivity) -> Option<UiKey> {
    match activity.source {
        ConversationSource::Network { server_id, .. } => Some(UiKey::Server(server_id)),
        ConversationSource::Client { client_id } => Some(UiKey::Client(client_id)),
        _ => None,
    }
}

pub fn connection(activity: &LlmActivity) -> Option<u32> {
    match activity.source {
        ConversationSource::Network { connection_id, .. } => connection_id.map(|id| id.as_u32()),
        _ => None,
    }
}

pub fn label(activity: &LlmActivity) -> String {
    let elapsed = human_duration(activity.started_at.elapsed().as_secs());
    let details = activity
        .details
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if details.is_empty() {
        format!("LLM generating · {elapsed}")
    } else {
        format!("LLM generating · {elapsed} · {details}")
    }
}
