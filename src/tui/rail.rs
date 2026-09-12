//! The instance list: one line per server and client.
//!
//! The list is the overview and stays one line per instance whatever is
//! happening inside it; depth lives in the inspector. One row at the foot
//! starts a new instance of either kind — the picker lists servers and
//! clients together, so there is nothing to choose before choosing.

use crate::tui::app::{Section, UiKey};
use crate::tui::driver::{driver_of, Driver};
use crate::tui::projection::{ClientRow, RailSnapshot, ServerRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
    Header(Section, usize),
    Instance(UiKey),
    /// `+ new server or client`, at the foot.
    New,
}

/// Servers, then clients, each under its header; the `+ new` row last.
pub fn list_rows(snapshot: &RailSnapshot) -> Vec<ListRow> {
    let mut rows = Vec::with_capacity(snapshot.servers.len() + snapshot.clients.len() + 3);
    rows.push(ListRow::Header(Section::Servers, snapshot.servers.len()));
    rows.extend(
        snapshot
            .servers
            .iter()
            .map(|s| ListRow::Instance(UiKey::Server(s.id))),
    );
    rows.push(ListRow::Header(Section::Clients, snapshot.clients.len()));
    rows.extend(
        snapshot
            .clients
            .iter()
            .map(|c| ListRow::Instance(UiKey::Client(c.id))),
    );
    rows.push(ListRow::New);
    rows
}

/// Rows the cursor can land on: instances and the `+ new` row.
pub fn is_selectable(row: &ListRow) -> bool {
    !matches!(row, ListRow::Header(..))
}

/// How a piece of text should be coloured, resolved by the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Dim,
    Good,
    Warn,
    Bad,
    Accent,
    Server,
    Client,
    Title,
    Reasoning,
}

impl Driver {
    pub fn tone(&self) -> Tone {
        match self {
            Driver::Manual => Tone::Warn,
            Driver::Llm => Tone::Reasoning,
            Driver::Silent => Tone::Dim,
            Driver::Rules => Tone::Good,
        }
    }
}

/// Everything one instance line shows, before it is fitted to a width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLine {
    pub key: UiKey,
    pub glyph: &'static str,
    pub glyph_tone: Tone,
    pub id: u32,
    pub protocol: String,
    /// `:8080` for a server, `→ host:port` for a client.
    pub target: String,
    /// `2⇄` live peers for a server; empty for a client.
    pub peers: String,
    pub driver: Driver,
    /// Requests parked for the human.
    pub waiting: usize,
    /// The error message when the instance is in an error state.
    pub error: Option<String>,
}

pub fn server_line(row: &ServerRow) -> InstanceLine {
    use crate::state::server::ServerStatus;
    let (glyph, glyph_tone, error) = match &row.status {
        ServerStatus::Running => ("●", Tone::Good, None),
        ServerStatus::Starting => ("◐", Tone::Warn, None),
        ServerStatus::Stopped => ("○", Tone::Dim, None),
        ServerStatus::Error(e) => ("✗", Tone::Bad, Some(e.clone())),
    };
    let live = row.conns.iter().filter(|c| c.active).count();
    InstanceLine {
        key: UiKey::Server(row.id),
        glyph,
        glyph_tone,
        id: row.id.as_u32(),
        protocol: row.protocol.to_lowercase(),
        target: row
            .local_addr
            .as_deref()
            .and_then(|a| a.rsplit_once(':').map(|(_, p)| format!(":{p}")))
            .unwrap_or_else(|| format!(":{}", row.port)),
        peers: format!("{live}⇄"),
        driver: driver_of(row.routing.as_ref()),
        waiting: row.intercepts.len(),
        error,
    }
}

pub fn client_line(row: &ClientRow) -> InstanceLine {
    use crate::state::client::ClientStatus;
    let (glyph, glyph_tone, error) = match &row.status {
        ClientStatus::Connected => ("●", Tone::Good, None),
        ClientStatus::Connecting => ("◐", Tone::Warn, None),
        ClientStatus::Disconnected => ("○", Tone::Dim, None),
        ClientStatus::Error(e) => ("✗", Tone::Bad, Some(e.clone())),
    };
    InstanceLine {
        key: UiKey::Client(row.id),
        glyph,
        glyph_tone,
        id: row.id.as_u32(),
        protocol: row.protocol.to_lowercase(),
        target: format!("→{}", row.remote_addr),
        peers: String::new(),
        driver: driver_of(row.routing.as_ref()),
        waiting: row.intercepts.len(),
        error,
    }
}

/// The line for whichever instance `key` names.
pub fn line_for(snapshot: &RailSnapshot, key: UiKey) -> Option<InstanceLine> {
    match key {
        UiKey::Server(id) => snapshot
            .servers
            .iter()
            .find(|s| s.id == id)
            .map(server_line),
        UiKey::Client(id) => snapshot
            .clients
            .iter()
            .find(|c| c.id == id)
            .map(client_line),
    }
}

/// Truncate to `width` columns with an ellipsis, counting chars.
pub fn fit(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width - 1).collect();
    out.push('…');
    out
}
