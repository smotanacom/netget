//! Dashboard application state: where the cursor is, what the last poll saw,
//! and which modals are open.

use std::collections::HashMap;

use crate::cli::input_state::InputState;
use crate::state::{ClientId, ServerId};
use crate::tui::activity::{Activity, ActivityFeed, ActivityKind, Tracker};
use crate::tui::cards::{CardState, Row};
use crate::tui::chat::EntryKind;
use crate::tui::hit::HitRegistry;
use crate::tui::metrics::Throughput;
use crate::tui::modal::Modal;
use crate::tui::projection::{ClientRow, RailSnapshot, ServerRow};
use crate::tui::theme::Styles;
use crate::ui::App;

/// Stable identity of an instance across re-polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UiKey {
    Server(ServerId),
    Client(ClientId),
}

impl UiKey {
    pub fn section(&self) -> Section {
        match self {
            UiKey::Server(_) => Section::Servers,
            UiKey::Client(_) => Section::Clients,
        }
    }

    /// `server #1` / `client #4`, for stream lines and titles.
    pub fn describe(&self) -> String {
        match self {
            UiKey::Server(id) => format!("server #{}", id.as_u32()),
            UiKey::Client(id) => format!("client #{}", id.as_u32()),
        }
    }

    pub fn raw_id(&self) -> u32 {
        match self {
            UiKey::Server(id) => id.as_u32(),
            UiKey::Client(id) => id.as_u32(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Servers,
    Clients,
}

/// Where keyboard input goes when no modal is open.
///
/// Two columns. `Cards` is the whole management column — every server and
/// client, their buttons and sections — walked with the arrows. `ChatInput`
/// is the box at the bottom of the stream; `Stream` is that column scrolled
/// away from its tail (PageUp, the wheel), where the arrows move through
/// the lines. Tab hops between the columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Cards,
    Stream,
    ChatInput,
}

/// The management column's cursor and fold state.
#[derive(Debug, Default)]
pub struct CardsUi {
    /// The row the cursor is on (index into `cards::rows`).
    pub row: usize,
    /// The position on that row: the label (0, where the label acts) then
    /// each button.
    pub col: usize,
    /// First visible row.
    pub scroll: usize,
    /// Inner width at the last paint, so the keymap builds the same rows
    /// the renderer did (button grids depend on it).
    pub width: usize,
    pub state: CardState,
    /// Throughput history per instance, sampled once a second.
    pub metrics: HashMap<UiKey, Throughput>,
}

/// Status-bar model.
#[derive(Debug, Clone, Default)]
pub struct StatusModel {
    pub model: String,
    pub backend: String,
    pub web_search: String,
    pub handler_mode: String,
    pub scripting: String,
    pub notice: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub llm_calls: u64,
    pub active_conversations: usize,
}

/// A borrowed view of either kind of instance row.
#[derive(Debug, Clone, Copy)]
pub enum InstanceRef<'a> {
    Server(&'a ServerRow),
    Client(&'a ClientRow),
}

impl<'a> InstanceRef<'a> {
    pub fn key(&self) -> UiKey {
        match self {
            InstanceRef::Server(s) => UiKey::Server(s.id),
            InstanceRef::Client(c) => UiKey::Client(c.id),
        }
    }

    pub fn protocol(&self) -> &str {
        match self {
            InstanceRef::Server(s) => &s.protocol,
            InstanceRef::Client(c) => &c.protocol,
        }
    }

    pub fn routing(&self) -> Option<&crate::scripting::EventHandlerConfig> {
        match self {
            InstanceRef::Server(s) => s.routing.as_ref(),
            InstanceRef::Client(c) => c.routing.as_ref(),
        }
    }

    pub fn intercepts(&self) -> &[crate::state::intercepts::InterceptView] {
        match self {
            InstanceRef::Server(s) => &s.intercepts,
            InstanceRef::Client(c) => &c.intercepts,
        }
    }

    pub fn requests(&self) -> &[crate::state::app_state::AccessLogEntry] {
        match self {
            InstanceRef::Server(s) => &s.requests,
            InstanceRef::Client(c) => &c.requests,
        }
    }

    /// `http#1`-style tag used by the stream.
    pub fn tag(&self) -> String {
        format!("{}#{}", self.protocol().to_lowercase(), self.key().raw_id())
    }
}

pub struct DashboardApp {
    /// Legacy display state reused for command history, log level and caps.
    pub core: App,
    /// The one timeline: machine events and the conversation.
    pub activity: ActivityFeed,
    pub tracker: Tracker,
    pub input: InputState,
    pub focus: Focus,
    pub cards: CardsUi,
    pub snapshot: RailSnapshot,
    pub modals: Vec<Modal>,
    pub hits: HitRegistry,
    pub status: StatusModel,
    pub styles: Styles,
    pub dirty: bool,
    pub mouse_capture: bool,
    pub should_quit: bool,
    /// Clone of the status channel, so modal actions (create/update/send) can
    /// stream their progress into the same pane.
    pub status_tx: tokio::sync::mpsc::UnboundedSender<String>,
    /// Results of spawned actions (see `crate::tui::uimsg`). Network work must
    /// never be awaited on the event loop.
    pub ui_tx: tokio::sync::mpsc::UnboundedSender<crate::tui::uimsg::UiMsg>,
    /// The configured LLM client, needed when creating/updating clients.
    pub llm_client: crate::llm::OllamaClient,
}

impl DashboardApp {
    pub fn new(
        core: App,
        styles: Styles,
        status_tx: tokio::sync::mpsc::UnboundedSender<String>,
        ui_tx: tokio::sync::mpsc::UnboundedSender<crate::tui::uimsg::UiMsg>,
        llm_client: crate::llm::OllamaClient,
    ) -> Self {
        Self {
            status_tx,
            ui_tx,
            llm_client,
            core,
            activity: ActivityFeed::new(),
            tracker: Tracker::default(),
            input: InputState::new(),
            focus: Focus::ChatInput,
            cards: CardsUi {
                width: 60,
                ..Default::default()
            },
            snapshot: RailSnapshot::default(),
            modals: Vec::new(),
            hits: HitRegistry::default(),
            status: StatusModel::default(),
            styles,
            dirty: true,
            mouse_capture: true,
            should_quit: false,
        }
    }

    pub fn modal(&self) -> Option<&Modal> {
        self.modals.last()
    }

    pub fn modal_mut(&mut self) -> Option<&mut Modal> {
        self.modals.last_mut()
    }

    /// The management column's rows as the renderer last laid them out.
    pub fn rows(&self) -> Vec<Row> {
        crate::tui::cards::rows(
            &self.snapshot,
            &self.cards.state,
            &self.cards.metrics,
            self.cards.width,
        )
    }

    /// Take a freshly built snapshot: derive activity from the change, keep
    /// the cursor on a row that exists, drop per-instance state for
    /// instances that are gone.
    pub fn absorb_snapshot(&mut self, snapshot: RailSnapshot) {
        let events = self.tracker.diff(&snapshot);
        for event in events {
            self.activity.push(event);
        }
        let newest = self.newest_arrival(&snapshot);
        self.snapshot = snapshot;
        self.prune();
        // An instance that just appeared gets the cursor: the thing you just
        // made is what you want to look at, and its buttons are one ↓ away.
        if let Some(key) = newest {
            self.focus_card(key);
        }
        let rows = self.rows();
        self.clamp_cursor_to(&rows);
        self.dirty = true;
    }

    /// The highest-id instance in `next` that the current snapshot lacks.
    fn newest_arrival(&self, next: &RailSnapshot) -> Option<UiKey> {
        let server = next
            .servers
            .iter()
            .filter(|s| !self.snapshot.servers.iter().any(|old| old.id == s.id))
            .map(|s| s.id)
            .max_by_key(|id| id.as_u32())
            .map(UiKey::Server);
        let client = next
            .clients
            .iter()
            .filter(|c| !self.snapshot.clients.iter().any(|old| old.id == c.id))
            .map(|c| c.id)
            .max_by_key(|id| id.as_u32())
            .map(UiKey::Client);
        client.or(server)
    }

    /// Put the cursor on `key`'s header row.
    pub fn focus_card(&mut self, key: UiKey) {
        let rows = self.rows();
        if let Some(index) = crate::tui::cards::header_index(&rows, key) {
            self.cards.row = index;
            self.cards.col = 0;
        }
    }

    /// Record one throughput sample per instance. Called on the 1s stats
    /// tick, so a sample is bytes per second.
    pub fn sample_metrics(&mut self) {
        for server in &self.snapshot.servers {
            let (mut rx, mut tx) = (0u64, 0u64);
            for c in &server.conns {
                rx += c.bytes_received;
                tx += c.bytes_sent;
            }
            for c in &server.recent {
                rx += c.bytes_received;
                tx += c.bytes_sent;
            }
            self.cards
                .metrics
                .entry(UiKey::Server(server.id))
                .or_default()
                .sample(rx, tx);
        }
        for client in &self.snapshot.clients {
            let (rx, tx) = client
                .connection
                .as_ref()
                .map(|c| (c.bytes_received, c.bytes_sent))
                .unwrap_or((0, 0));
            self.cards
                .metrics
                .entry(UiKey::Client(client.id))
                .or_default()
                .sample(rx, tx);
        }
    }

    fn prune(&mut self) {
        let live: std::collections::HashSet<UiKey> = self
            .snapshot
            .servers
            .iter()
            .map(|s| UiKey::Server(s.id))
            .chain(self.snapshot.clients.iter().map(|c| UiKey::Client(c.id)))
            .collect();
        self.cards.metrics.retain(|key, _| live.contains(key));
    }

    /// Keep the cursor on a row that can take it.
    ///
    /// Rows come and go under the cursor — a peer closes, a card folds, a
    /// server stops — so the index is re-checked against the rows as they
    /// are now: forward to the next stop, else back to the previous one.
    pub fn clamp_cursor_to(&mut self, rows: &[Row]) {
        if rows.is_empty() {
            self.cards.row = 0;
            self.cards.col = 0;
            return;
        }
        let row = self.cards.row.min(rows.len() - 1);
        let stop = (row..rows.len())
            .find(|i| rows[*i].positions() > 0)
            .or_else(|| (0..row).rev().find(|i| rows[*i].positions() > 0))
            .unwrap_or(0);
        self.cards.row = stop;
        let positions = rows[stop].positions();
        if positions == 0 {
            self.cards.col = 0;
        } else if self.cards.col >= positions {
            self.cards.col = positions - 1;
        }
    }

    /// The instance whose row the cursor is on, if any.
    pub fn cursor_key(&self) -> Option<UiKey> {
        self.rows().get(self.cards.row).and_then(|r| r.key)
    }

    pub fn server_row(&self, id: ServerId) -> Option<&ServerRow> {
        self.snapshot.servers.iter().find(|s| s.id == id)
    }

    pub fn client_row(&self, id: ClientId) -> Option<&ClientRow> {
        self.snapshot.clients.iter().find(|c| c.id == id)
    }

    pub fn instance(&self, key: UiKey) -> Option<InstanceRef<'_>> {
        match key {
            UiKey::Server(id) => self.server_row(id).map(InstanceRef::Server),
            UiKey::Client(id) => self.client_row(id).map(InstanceRef::Client),
        }
    }

    /// Pending intercepts across every instance.
    pub fn waiting_count(&self) -> usize {
        self.snapshot
            .servers
            .iter()
            .map(|s| s.intercepts.len())
            .sum::<usize>()
            + self
                .snapshot
                .clients
                .iter()
                .map(|c| c.intercepts.len())
                .sum::<usize>()
    }

    /// A conversation entry into the stream.
    pub fn push_chat(&mut self, kind: EntryKind, text: impl Into<String>) {
        let kind = match kind {
            EntryKind::Log(level) => ActivityKind::Log(level),
            other => ActivityKind::Chat(other),
        };
        self.activity.push(Activity {
            kind,
            owner: None,
            tag: String::new(),
            text: text.into(),
            link: None,
            at_unix_ms: None,
        });
        self.dirty = true;
    }

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.push_chat(EntryKind::System, text);
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.push_chat(EntryKind::Log(crate::ui::app::LogLevel::Error), text);
    }
}
