//! Pretty-printed view of one access-log entry: who handled it, the request
//! payload, and the actions that answered it.

use crate::state::app_state::AccessLogEntry;

/// Render the entry as display lines (header, request JSON, response JSON).
pub fn detail_lines(entry: &AccessLogEntry) -> Vec<String> {
    let mut lines = Vec::new();
    let owner = match (entry.server_id, entry.client_id) {
        (Some(id), _) => format!("server #{id}"),
        (None, Some(id)) => format!("client #{id}"),
        (None, None) => "unknown".to_string(),
    };
    lines.push(format!("id        {}", entry.id));
    lines.push(format!("owner     {owner}"));
    lines.push(format!("protocol  {}", entry.protocol));
    lines.push(format!("event     {}", entry.event_type));
    lines.push(format!(
        "conn      {}",
        entry
            .connection_id
            .map(|c| c.to_string())
            .unwrap_or_else(|| "(connectionless)".to_string())
    ));
    lines.push(format!("when      unix_ms {}", entry.unix_ms));
    lines.push(String::new());

    lines.push("── request ──".to_string());
    let request =
        serde_json::to_string_pretty(&entry.request).unwrap_or_else(|_| entry.request.to_string());
    lines.extend(request.lines().map(|l| l.to_string()));
    lines.push(String::new());

    lines.push(format!(
        "── response ({} action(s)) ──",
        entry.response.len()
    ));
    if entry.response.is_empty() {
        lines.push("(no actions)".to_string());
    } else {
        let response =
            serde_json::to_string_pretty(&entry.response).unwrap_or_else(|_| "[]".to_string());
        lines.extend(response.lines().map(|l| l.to_string()));
    }
    lines
}

/// One-line summary for a request row: `#id event → answer`.
pub fn summary_line(entry: &AccessLogEntry) -> String {
    format!(
        "#{} {} → {}",
        entry.id,
        entry.event_type,
        answer_summary(entry)
    )
}

/// What answered the request, in a word: the first action's type (plus
/// `+N` for the rest), `—` when nothing did.
pub fn answer_summary(entry: &AccessLogEntry) -> String {
    let action = match entry.response.first() {
        None => "—".to_string(),
        Some(first) => first
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| t.to_string())
            // Injected-action entries record a ClientSendOutcome, which is an
            // externally-tagged enum: its single key names the outcome.
            .or_else(|| {
                first
                    .as_object()
                    .and_then(|o| o.keys().next().cloned())
                    .map(|k| k.to_lowercase())
            })
            .or_else(|| first.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "?".to_string()),
    };
    let extra = if entry.response.len() > 1 {
        format!(" +{}", entry.response.len() - 1)
    } else {
        String::new()
    };
    format!("{action}{extra}")
}
