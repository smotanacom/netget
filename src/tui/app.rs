//! Dashboard application state: what is focused, what is selected, what the
//! last poll saw, and which modals are open.

use std::collections::HashMap;

use crate::cli::input_state::InputState;
use crate::state::{ClientId, ServerId};
use crate::tui::activity::{ActivityFeed, Tracker};
use crate::tui::chat::ChatState;
use crate::tui::hit::HitRegistry;
use crate::tui::inspector::InspectorTab;
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

    /// `server #1` / `client #4`, for chat lines and titles.
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
/// Tab walks Instances → Inspector → Activity → ChatInput; Esc steps back
/// towards typing. `ChatHistory` is the chat pane scrolled away from its
/// tail — the same pane, in a mode where the arrows move the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Instances,
    Inspector,
    Activity,
    ChatInput,
    ChatHistory,
}

/// The instance list's own state.
#[derive(Debug, Default)]
pub struct InstancesUi {
    /// The selected row, by identity rather than index, so a re-poll that
    /// reorders or removes instances cannot silently move the cursor onto a
    /// different one.
    pub selected: Option<UiKey>,
    /// Cursor on the `+ new server or client` row, which belongs to no
    /// instance.
    pub on_new: bool,
    /// Where the cursor last was, so a vanished instance hands the cursor to
    /// its neighbour rather than to nothing.
    pub last_index: usize,
    /// First visible row.
    pub scroll: usize,
    /// Throughput history per instance, sampled once a second.
    pub metrics: HashMap<UiKey, Throughput>,
}

/// Which peer the traffic tab is narrowed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrafficFilter {
    #[default]
    All,
    /// One connection; `None` is the connectionless bucket.
    Peer(Option<u32>),
}

/// The inspector's own state. Tab and item survive moving between instances
/// on purpose: comparing two servers' traffic means pressing ↓, not ↓ then
/// re-finding the tab.
#[derive(Debug)]
pub struct InspectorUi {
    pub tab: InspectorTab,
    /// Selected item (index into the tab's selectable items).
    pub item: usize,
    /// First visible body line.
    pub scroll: usize,
    pub filter: TrafficFilter,
}

impl Default for InspectorUi {
    fn default() -> Self {
        Self {
            tab: InspectorTab::Overview,
            item: 0,
            scroll: 0,
            filter: TrafficFilter::All,
        }
    }
}

/// How the right column is split between the feed and the chat.
///
/// Balanced sizes the chat to its conversation. The two maximised modes give
/// one pane the whole column — reading a long model answer, or watching a
/// busy server — and F2 cycles through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RightLayout {
    #[default]
    Balanced,
    ChatMax,
    FeedMax,
}

impl RightLayout {
    pub fn next(&self) -> Self {
        match self {
            RightLayout::Balanced => RightLayout::ChatMax,
            RightLayout::ChatMax => RightLayout::FeedMax,
            RightLayout::FeedMax => RightLayout::Balanced,
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            RightLayout::Balanced => "feed and chat share the column",
            RightLayout::ChatMax => "chat takes the column (F2 again for the feed)",
            RightLayout::FeedMax => "feed takes the column (F2 again to balance)",
        }
    }
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

    /// `http#1`-style tag used by the feed and the inspector title.
    pub fn tag(&self) -> String {
        format!("{}#{}", self.protocol().to_lowercase(), self.key().raw_id())
    }
}

pub struct DashboardApp {
    /// Legacy display state reused for command history, log level and caps.
    pub core: App,
    pub chat: ChatState,
    pub activity: ActivityFeed,
    pub tracker: Tracker,
    pub input: InputState,
    pub focus: Focus,
    pub instances: InstancesUi,
    pub inspector: InspectorUi,
    pub snapshot: RailSnapshot,
    pub modals: Vec<Modal>,
    pub hits: HitRegistry,
    pub status: StatusModel,
    pub styles: Styles,
    pub right_layout: RightLayout,
    pub dirty: bool,
    pub mouse_capture: bool,
    pub should_quit: bool,
    /// Clone of the status channel, so modal actions (create/update/send) can
    /// stream their progress into the same panes.
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
            chat: ChatState::new(),
            activity: ActivityFeed::new(),
            tracker: Tracker::default(),
            input: InputState::new(),
            focus: Focus::ChatInput,
            instances: InstancesUi::default(),
            inspector: InspectorUi::default(),
            snapshot: RailSnapshot::default(),
            modals: Vec::new(),
            hits: HitRegistry::default(),
            status: StatusModel::default(),
            styles,
            right_layout: RightLayout::default(),
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

    /// Take a freshly built snapshot: derive activity from the change, keep
    /// the selection meaningful, drop per-instance state for instances that
    /// are gone.
    pub fn absorb_snapshot(&mut self, snapshot: RailSnapshot) {
        let events = self.tracker.diff(&snapshot);
        for event in events {
            self.activity.push(event);
        }
        let newest = self.newest_arrival(&snapshot);
        self.snapshot = snapshot;
        self.prune();
        // An instance that just appeared becomes the selection when nothing
        // else is being looked at — including when the cursor sits on the
        // `+ new …` row that created it. Otherwise Enter after "start a tcp
        // server" reopened the picker, and the thing you just made sat one
        // row above, unselected.
        if let Some(key) = newest {
            if self.instances.selected.is_none() || self.instances.on_new {
                self.select(key);
            }
        }
        self.clamp_selection();
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
        // A client made from a server's `[ + client ]` is the newer of the two.
        client.or(server)
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
            self.instances
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
            self.instances
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
        self.instances.metrics.retain(|key, _| live.contains(key));
    }

    /// Keep the selection pointing at something that exists.
    ///
    /// A stopped server or a removed client takes its row with it; the cursor
    /// moves to the row that now occupies its place (or the last one), not to
    /// nothing. With no instances at all, the cursor sits on `+ new server`
    /// when the list is focused, so Enter always does something.
    pub fn clamp_selection(&mut self) {
        use crate::tui::rail::{list_rows, ListRow};
        let rows = list_rows(&self.snapshot);
        let instance_rows: Vec<(usize, UiKey)> = rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                ListRow::Instance(key) => Some((i, *key)),
                _ => None,
            })
            .collect();

        if let Some(key) = self.instances.selected {
            if let Some((index, _)) = instance_rows.iter().find(|(_, k)| *k == key) {
                self.instances.last_index = *index;
                self.instances.on_new = false;
                return;
            }
            // Gone: hand the cursor to the neighbour that now sits where it was.
            let replacement = instance_rows
                .iter()
                .filter(|(i, _)| *i >= self.instances.last_index)
                .min_by_key(|(i, _)| *i)
                .or_else(|| instance_rows.last())
                .map(|(i, k)| (*i, *k));
            match replacement {
                Some((index, key)) => {
                    self.instances.selected = Some(key);
                    self.instances.last_index = index;
                    self.instances.on_new = false;
                    self.inspector.item = 0;
                    self.inspector.scroll = 0;
                    self.inspector.filter = TrafficFilter::All;
                }
                None => {
                    self.instances.selected = None;
                    if self.focus == Focus::Inspector {
                        self.focus = Focus::Instances;
                    }
                    self.instances.on_new = true;
                }
            }
            return;
        }

        // Nothing selected. When the list (or inspector) is focused, a cursor
        // must exist: the first instance, else `+ new server`.
        if !self.instances.on_new {
            if let Some((index, key)) = instance_rows.first() {
                if matches!(self.focus, Focus::Instances | Focus::Inspector) {
                    self.instances.selected = Some(*key);
                    self.instances.last_index = *index;
                }
            } else if self.focus == Focus::Instances {
                self.instances.on_new = true;
            }
        }
        if self.instances.selected.is_none() && self.focus == Focus::Inspector {
            self.focus = Focus::Instances;
        }
    }

    /// Select an instance and reset the inspector's cursor for it.
    pub fn select(&mut self, key: UiKey) {
        if self.instances.selected != Some(key) {
            self.inspector.item = 0;
            self.inspector.scroll = 0;
            self.inspector.filter = TrafficFilter::All;
        }
        self.instances.selected = Some(key);
        self.instances.on_new = false;
        self.clamp_selection();
    }

    pub fn selected(&self) -> Option<UiKey> {
        self.instances.selected
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

    pub fn selected_instance(&self) -> Option<InstanceRef<'_>> {
        self.selected().and_then(|key| self.instance(key))
    }

    /// Pending intercepts across every instance, oldest first.
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

    pub fn push_system(&mut self, text: impl Into<String>) {
        self.chat.push(crate::tui::chat::EntryKind::System, text);
        self.dirty = true;
    }

    pub fn push_error(&mut self, text: impl Into<String>) {
        self.chat.push(
            crate::tui::chat::EntryKind::Log(crate::ui::app::LogLevel::Error),
            text,
        );
        self.dirty = true;
    }
}
