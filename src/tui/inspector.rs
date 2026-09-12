//! The inspector: the selected instance in depth, one tab at a time.
//!
//! Every tab produces a list of lines, some of which are *items* the cursor
//! can rest on, and an action bar whose buttons act on the instance or on the
//! selected item. The renderer draws; the keymap moves the cursor; everything
//! that *does* something is an [`InstanceAction`] run by `actions::run`, so
//! a letter, Enter on a button and a click on it cannot diverge.

use crate::tui::app::{InspectorUi, InstanceRef, TrafficFilter, UiKey};
use crate::tui::driver::{driver_of, specific_rule_count};
use crate::tui::metrics::{clock, human_bytes, human_duration, human_rate, Throughput};
use crate::tui::projection::SendState;
use crate::tui::rail::{fit, Tone};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InspectorTab {
    Overview,
    /// A server's connections; a client's connection history.
    Peers,
    Traffic,
    Rules,
    Config,
    /// A client's own verbs.
    Send,
}

impl InspectorTab {
    /// The tabs an instance offers, in strip order.
    pub fn for_key(key: UiKey) -> Vec<InspectorTab> {
        match key {
            UiKey::Server(_) => vec![
                InspectorTab::Overview,
                InspectorTab::Peers,
                InspectorTab::Traffic,
                InspectorTab::Rules,
                InspectorTab::Config,
            ],
            UiKey::Client(_) => vec![
                InspectorTab::Overview,
                InspectorTab::Send,
                InspectorTab::Peers,
                InspectorTab::Traffic,
                InspectorTab::Rules,
                InspectorTab::Config,
            ],
        }
    }

    pub fn label(&self, key: UiKey) -> &'static str {
        match (self, key) {
            (InspectorTab::Overview, _) => "overview",
            (InspectorTab::Peers, UiKey::Server(_)) => "peers",
            (InspectorTab::Peers, UiKey::Client(_)) => "connections",
            (InspectorTab::Traffic, _) => "traffic",
            (InspectorTab::Rules, _) => "rules",
            (InspectorTab::Config, _) => "config",
            (InspectorTab::Send, _) => "send",
        }
    }
}

/// Everything the dashboard can do to an instance. Produced by letters, bar
/// buttons and clicks alike; executed in `actions.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstanceAction {
    /// Stop a server / remove a client.
    Stop,
    /// Open the config form.
    Edit,
    /// Open the routing editor on the table.
    Rules,
    /// Open the routing editor on a fresh rule.
    AddRule,
    /// Open the routing editor on one rule.
    EditRule(usize),
    /// Delete one rule and apply.
    DeleteRule(usize),
    /// Move one rule earlier (-1) or later (+1) and apply.
    MoveRule(usize, i8),
    /// Cycle MANUAL → LLM → SILENT and apply.
    CycleDriver,
    /// Connect a client of the counterpart protocol to this server.
    ConnectClient,
    /// Open the composer on the action list.
    Send,
    /// Open the composer on one action, by index into `send_actions`.
    SendAction(usize),
    /// Compose for one live server connection.
    MessagePeer(u32),
    /// Close one live server connection from our side.
    DisconnectPeer(u32),
    /// Narrow the traffic tab to one peer.
    FilterTraffic(Option<u32>),
    ClearTrafficFilter,
    /// Hang up a client but keep it.
    Disconnect,
    /// Redial a disconnected client.
    Connect,
    Wireshark,
    /// Protocol description and maturity, into chat.
    Docs,
    /// Open the answer modal for a parked request.
    Answer(u64),
    /// Open the request/response detail for one access-log entry.
    OpenRequest(u64),
}

/// One action-bar button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarButton {
    pub action: InstanceAction,
    pub label: String,
    /// A disabled button stays visible (so the capability is discoverable)
    /// and says why when pressed.
    pub enabled: bool,
    pub why_disabled: Option<String>,
}

impl BarButton {
    fn on(action: InstanceAction, label: impl Into<String>) -> Self {
        Self {
            action,
            label: label.into(),
            enabled: true,
            why_disabled: None,
        }
    }

    fn off(action: InstanceAction, label: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            action,
            label: label.into(),
            enabled: false,
            why_disabled: Some(why.into()),
        }
    }
}

/// What a selectable line stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Intercept(u64),
    /// A server connection; `None` is the connectionless bucket.
    Peer(Option<u32>),
    /// A client connection attempt, by start time.
    Attempt(u64),
    Request(u64),
    Rule(usize),
    Config(String),
    SendAction(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorLine {
    pub item: Option<Item>,
    pub spans: Vec<(String, Tone)>,
}

impl InspectorLine {
    fn note(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            item: None,
            spans: vec![(text.into(), tone)],
        }
    }

    fn field(label: &str, value: impl Into<String>, tone: Tone) -> Self {
        Self {
            item: None,
            spans: vec![(format!("{label:<10}"), Tone::Dim), (value.into(), tone)],
        }
    }

    fn item(item: Item, spans: Vec<(String, Tone)>) -> Self {
        Self {
            item: Some(item),
            spans,
        }
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|(s, _)| s.as_str()).collect()
    }
}

/// The inspector, computed for one instance from the snapshot and its UI state.
#[derive(Debug, Clone)]
pub struct InspectorView {
    pub key: UiKey,
    pub title: String,
    pub tabs: Vec<InspectorTab>,
    pub tab: InspectorTab,
    pub bar: Vec<BarButton>,
    pub lines: Vec<InspectorLine>,
}

impl InspectorView {
    /// Indices of the selectable lines.
    pub fn item_lines(&self) -> Vec<usize> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.item.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn item_count(&self) -> usize {
        self.lines.iter().filter(|l| l.item.is_some()).count()
    }

    /// The n-th selectable item.
    pub fn item_at(&self, n: usize) -> Option<&Item> {
        self.lines.iter().filter_map(|l| l.item.as_ref()).nth(n)
    }

    /// What Enter does on the n-th item.
    pub fn default_action(&self, n: usize) -> Option<InstanceAction> {
        match self.item_at(n)? {
            Item::Intercept(id) => Some(InstanceAction::Answer(*id)),
            Item::Peer(conn) => Some(InstanceAction::FilterTraffic(*conn)),
            Item::Attempt(_) => None,
            Item::Request(id) => Some(InstanceAction::OpenRequest(*id)),
            Item::Rule(index) => Some(InstanceAction::EditRule(*index)),
            Item::Config(_) => Some(InstanceAction::Edit),
            Item::SendAction(index) => Some(InstanceAction::SendAction(*index)),
        }
    }
}

/// Resolve the tab the UI asks for against what this instance offers.
pub fn effective_tab(key: UiKey, wanted: InspectorTab) -> InspectorTab {
    if InspectorTab::for_key(key).contains(&wanted) {
        wanted
    } else {
        InspectorTab::Overview
    }
}

pub fn build(
    instance: InstanceRef<'_>,
    ui: &InspectorUi,
    metrics: Option<&Throughput>,
    width: usize,
) -> InspectorView {
    let key = instance.key();
    let tab = effective_tab(key, ui.tab);
    let tabs = InspectorTab::for_key(key);
    let title = match instance {
        InstanceRef::Server(row) => format!(
            "#{} {} {}",
            row.id.as_u32(),
            row.protocol.to_lowercase(),
            row.local_addr
                .clone()
                .unwrap_or_else(|| format!(":{}", row.port))
        ),
        InstanceRef::Client(row) => format!(
            "#{} {} → {}",
            row.id.as_u32(),
            row.protocol.to_lowercase(),
            row.remote_addr
        ),
    };

    let lines = match tab {
        InspectorTab::Overview => overview_lines(instance, metrics, width),
        InspectorTab::Peers => peers_lines(instance),
        InspectorTab::Traffic => traffic_lines(instance, ui.filter, width),
        InspectorTab::Rules => rules_lines(instance),
        InspectorTab::Config => config_lines(instance, width),
        InspectorTab::Send => send_lines(instance, width),
    };

    let mut view = InspectorView {
        key,
        title,
        tabs,
        tab,
        bar: Vec::new(),
        lines,
    };
    let selected = view.item_at(ui.item).cloned();
    view.bar = bar_buttons(instance, tab, selected.as_ref(), ui.filter);
    view
}

fn driver_line(instance: InstanceRef<'_>) -> InspectorLine {
    let driver = driver_of(instance.routing());
    let specific = specific_rule_count(instance.routing());
    let suffix = match specific {
        0 => String::new(),
        1 => " · 1 specific rule first".to_string(),
        n => format!(" · {n} specific rules first"),
    };
    InspectorLine {
        item: None,
        spans: vec![
            ("driver    ".to_string(), Tone::Dim),
            (driver.label().to_string(), driver.tone()),
            (format!(" — {}{suffix}", driver.describe()), Tone::Normal),
        ],
    }
}

fn traffic_line(rx: u64, tx: u64, metrics: Option<&Throughput>, requests: usize) -> InspectorLine {
    let mut value = format!("↓{} ↑{}", human_bytes(rx), human_bytes(tx));
    if let Some(m) = metrics {
        let (rrx, rtx) = m.rate();
        if rrx > 0 || rtx > 0 {
            value.push_str(&format!(" · now ↓{} ↑{}", human_rate(rrx), human_rate(rtx)));
        }
    }
    value.push_str(&format!(" · {requests} req"));
    InspectorLine::field("traffic", value, Tone::Normal)
}

fn intercept_lines(instance: InstanceRef<'_>, lines: &mut Vec<InspectorLine>) {
    for view in instance.intercepts() {
        let peer = match instance {
            InstanceRef::Server(row) => view
                .connection_id
                .and_then(|c| row.conns.iter().find(|x| x.id == c))
                .map(|c| {
                    // The port is what tells peers apart; the host is
                    // almost always the same for every one of them.
                    let port = c
                        .remote_addr
                        .rsplit_once(':')
                        .map(|(_, p)| format!(":{p}"))
                        .unwrap_or_else(|| c.remote_addr.clone());
                    format!(" from {port}")
                })
                .unwrap_or_default(),
            InstanceRef::Client(_) => String::new(),
        };
        lines.push(InspectorLine::item(
            Item::Intercept(view.id),
            vec![
                ("⚠ YOUR answer needed".to_string(), Tone::Bad),
                (format!(" · {}{peer}", view.event_type), Tone::Normal),
            ],
        ));
    }
}

fn overview_lines(
    instance: InstanceRef<'_>,
    metrics: Option<&Throughput>,
    width: usize,
) -> Vec<InspectorLine> {
    let mut lines = Vec::new();
    intercept_lines(instance, &mut lines);
    match instance {
        InstanceRef::Server(row) => {
            use crate::state::server::ServerStatus;
            let (status, tone) = match &row.status {
                ServerStatus::Running => ("Running".to_string(), Tone::Good),
                ServerStatus::Starting => ("Starting".to_string(), Tone::Warn),
                ServerStatus::Stopped => ("Stopped".to_string(), Tone::Dim),
                ServerStatus::Error(_) => ("Error".to_string(), Tone::Bad),
            };
            lines.push(InspectorLine {
                item: None,
                spans: vec![
                    ("status    ".to_string(), Tone::Dim),
                    (status, tone),
                    (
                        format!(" · up {}", human_duration(row.uptime_secs)),
                        Tone::Dim,
                    ),
                ],
            });
            if let ServerStatus::Error(e) = &row.status {
                lines.push(InspectorLine::field(
                    "error",
                    fit(e, width.saturating_sub(10)),
                    Tone::Bad,
                ));
            }
            lines.push(InspectorLine::field(
                "bound",
                row.local_addr
                    .clone()
                    .unwrap_or_else(|| format!("(requested port {})", row.port)),
                Tone::Accent,
            ));
            let live = row.conns.iter().filter(|c| c.active).count();
            let (rx, tx) = row.conns.iter().fold((0u64, 0u64), |(a, b), c| {
                (a + c.bytes_received, b + c.bytes_sent)
            });
            lines.push(traffic_line(rx, tx, metrics, row.requests.len()));
            lines.push(InspectorLine::field(
                "peers",
                format!("{live} live · {} recent", row.recent.len()),
                Tone::Normal,
            ));
            lines.push(driver_line(instance));
            if let Some(m) = metrics {
                let spark_width = width.saturating_sub(10).min(30);
                if !m.is_idle() && spark_width > 0 {
                    lines.push(InspectorLine::field(
                        "last 30s",
                        m.sparkline(spark_width),
                        Tone::Accent,
                    ));
                }
            }
            if row.task_count > 0 {
                lines.push(InspectorLine::field(
                    "tasks",
                    format!("{} scheduled", row.task_count),
                    Tone::Normal,
                ));
            }
            push_instruction(&mut lines, &row.instruction, width);
        }
        InstanceRef::Client(row) => {
            use crate::state::client::ClientStatus;
            let (status, tone) = match &row.status {
                ClientStatus::Connected => ("Connected".to_string(), Tone::Good),
                ClientStatus::Connecting => ("Connecting".to_string(), Tone::Warn),
                ClientStatus::Disconnected => ("Disconnected".to_string(), Tone::Dim),
                ClientStatus::Error(_) => ("Error".to_string(), Tone::Bad),
            };
            lines.push(InspectorLine {
                item: None,
                spans: vec![
                    ("status    ".to_string(), Tone::Dim),
                    (status, tone),
                    (
                        format!(" · up {}", human_duration(row.uptime_secs)),
                        Tone::Dim,
                    ),
                ],
            });
            if let ClientStatus::Error(e) = &row.status {
                lines.push(InspectorLine::field(
                    "error",
                    fit(e, width.saturating_sub(10)),
                    Tone::Bad,
                ));
            }
            lines.push(InspectorLine::field(
                "remote",
                row.remote_addr.clone(),
                Tone::Accent,
            ));
            let (rx, tx) = row
                .connection
                .as_ref()
                .map(|c| (c.bytes_received, c.bytes_sent))
                .unwrap_or((0, 0));
            lines.push(traffic_line(rx, tx, metrics, row.requests.len()));
            let (send, tone) = match row.send_state {
                SendState::Ready => (
                    format!("{} verb(s) — see the send tab", row.send_actions.len()),
                    Tone::Good,
                ),
                SendState::NotConnected => ("not connected".to_string(), Tone::Dim),
                SendState::ProtocolUnsupported => (
                    "this protocol's loop takes no injected actions yet".to_string(),
                    Tone::Dim,
                ),
            };
            lines.push(InspectorLine::field("send", send, tone));
            lines.push(driver_line(instance));
            if let Some(m) = metrics {
                let spark_width = width.saturating_sub(10).min(30);
                if !m.is_idle() && spark_width > 0 {
                    lines.push(InspectorLine::field(
                        "last 30s",
                        m.sparkline(spark_width),
                        Tone::Accent,
                    ));
                }
            }
            push_instruction(&mut lines, &row.instruction, width);
        }
    }
    lines
}

fn push_instruction(lines: &mut Vec<InspectorLine>, instruction: &str, width: usize) {
    let text = instruction.trim();
    if text.is_empty() {
        lines.push(InspectorLine::field(
            "brief",
            "(no instruction — the model has nothing to go on if it is asked)",
            Tone::Dim,
        ));
        return;
    }
    let first = text.lines().next().unwrap_or("");
    let more = text.lines().count().saturating_sub(1);
    let mut value = fit(first, width.saturating_sub(10));
    if more > 0 {
        value.push_str(&format!(" (+{more} lines)"));
    }
    lines.push(InspectorLine::field("brief", value, Tone::Normal));
}

fn peers_lines(instance: InstanceRef<'_>) -> Vec<InspectorLine> {
    let mut lines = Vec::new();
    match instance {
        InstanceRef::Server(row) => {
            let mut any = false;
            for conn in &row.conns {
                any = true;
                let requests = row
                    .requests
                    .iter()
                    .filter(|r| r.connection_id == Some(conn.id))
                    .count();
                let waiting = row
                    .intercepts
                    .iter()
                    .any(|v| v.connection_id == Some(conn.id));
                let mut spans = vec![
                    (
                        if conn.active { "● " } else { "○ " }.to_string(),
                        if conn.active { Tone::Good } else { Tone::Dim },
                    ),
                    (
                        conn.remote_addr.clone(),
                        if conn.active { Tone::Normal } else { Tone::Dim },
                    ),
                    (
                        format!(
                            "  ↓{} ↑{} · {requests} req",
                            human_bytes(conn.bytes_received),
                            human_bytes(conn.bytes_sent)
                        ),
                        Tone::Dim,
                    ),
                ];
                if !conn.active {
                    spans.push(("  (closed)".to_string(), Tone::Dim));
                }
                if waiting {
                    spans.push(("  ⚠ waiting for you".to_string(), Tone::Bad));
                }
                lines.push(InspectorLine::item(Item::Peer(Some(conn.id)), spans));
            }
            for closed in &row.recent {
                any = true;
                let requests = row
                    .requests
                    .iter()
                    .filter(|r| r.connection_id == Some(closed.id))
                    .count();
                lines.push(InspectorLine::item(
                    Item::Peer(Some(closed.id)),
                    vec![
                        ("○ ".to_string(), Tone::Dim),
                        (closed.remote_addr.clone(), Tone::Dim),
                        (
                            format!(
                                "  ↓{} ↑{} · {requests} req  (closed)",
                                human_bytes(closed.bytes_received),
                                human_bytes(closed.bytes_sent)
                            ),
                            Tone::Dim,
                        ),
                    ],
                ));
            }
            let connectionless = row
                .requests
                .iter()
                .filter(|r| r.connection_id.is_none())
                .count();
            if connectionless > 0 {
                any = true;
                lines.push(InspectorLine::item(
                    Item::Peer(None),
                    vec![
                        ("· ".to_string(), Tone::Dim),
                        ("(connectionless)".to_string(), Tone::Dim),
                        (format!("  {connectionless} req"), Tone::Dim),
                    ],
                ));
            }
            if !any {
                lines.push(InspectorLine::note(
                    format!(
                        "(no connections yet — listening on {})",
                        row.local_addr
                            .clone()
                            .unwrap_or_else(|| format!(":{}", row.port))
                    ),
                    Tone::Dim,
                ));
            }
            lines.push(InspectorLine::note(
                "Enter narrows the traffic tab to a peer.",
                Tone::Dim,
            ));
        }
        InstanceRef::Client(row) => {
            if row.history.is_empty() {
                match &row.connection {
                    Some(c) => lines.push(InspectorLine::item(
                        Item::Attempt(0),
                        vec![
                            ("● ".to_string(), Tone::Good),
                            (c.remote_addr.clone(), Tone::Normal),
                            (
                                format!(
                                    "  ↓{} ↑{}",
                                    human_bytes(c.bytes_received),
                                    human_bytes(c.bytes_sent)
                                ),
                                Tone::Dim,
                            ),
                        ],
                    )),
                    None => lines.push(InspectorLine::note("(no connections yet)", Tone::Dim)),
                }
            } else {
                let last = row.history.len() - 1;
                for (index, attempt) in row.history.iter().enumerate().rev() {
                    let live = index == last
                        && attempt.ended_unix_ms.is_none()
                        && row.connection.is_some();
                    let mut spans = vec![
                        (
                            if live { "● " } else { "○ " }.to_string(),
                            if live { Tone::Good } else { Tone::Dim },
                        ),
                        (
                            format!("{} {}", clock(attempt.started_unix_ms), attempt.remote_addr),
                            if live { Tone::Normal } else { Tone::Dim },
                        ),
                    ];
                    if live {
                        if let Some(c) = &row.connection {
                            spans.push((
                                format!(
                                    "  ↓{} ↑{}",
                                    human_bytes(c.bytes_received),
                                    human_bytes(c.bytes_sent)
                                ),
                                Tone::Dim,
                            ));
                        }
                    } else {
                        spans.push((format!("  {}", attempt.outcome), Tone::Dim));
                    }
                    lines.push(InspectorLine::item(
                        Item::Attempt(attempt.started_unix_ms),
                        spans,
                    ));
                }
            }
        }
    }
    lines
}

fn traffic_lines(
    instance: InstanceRef<'_>,
    filter: TrafficFilter,
    width: usize,
) -> Vec<InspectorLine> {
    let mut lines = Vec::new();
    let requests = instance.requests();
    let (server_conns, server_recent) = match instance {
        InstanceRef::Server(row) => (Some(&row.conns), Some(&row.recent)),
        InstanceRef::Client(_) => (None, None),
    };
    if let TrafficFilter::Peer(conn) = filter {
        let name = match conn {
            None => "(connectionless)".to_string(),
            Some(id) => server_conns
                .and_then(|c| c.iter().find(|x| x.id == id))
                .map(|c| c.remote_addr.clone())
                .or_else(|| {
                    server_recent
                        .and_then(|r| r.iter().find(|x| x.id == id))
                        .map(|c| c.remote_addr.clone())
                })
                .unwrap_or_else(|| format!("connection #{id}")),
        };
        lines.push(InspectorLine::note(
            format!("only peer {name} — [ all peers ] clears"),
            Tone::Dim,
        ));
    }
    let mut shown = 0;
    for entry in requests.iter().rev() {
        if let TrafficFilter::Peer(conn) = filter {
            if entry.connection_id != conn {
                continue;
            }
        }
        shown += 1;
        let peer = match instance {
            InstanceRef::Server(row) => entry
                .connection_id
                .and_then(|c| {
                    row.conns
                        .iter()
                        .find(|x| x.id == c)
                        .map(|x| x.remote_addr.clone())
                        .or_else(|| {
                            row.recent
                                .iter()
                                .find(|x| x.id == c)
                                .map(|x| x.remote_addr.clone())
                        })
                })
                .map(|addr| {
                    // The port is the part that tells peers apart.
                    addr.rsplit_once(':')
                        .map(|(_, p)| format!(":{p}"))
                        .unwrap_or(addr)
                })
                .unwrap_or_else(|| "·".to_string()),
            InstanceRef::Client(_) => String::new(),
        };
        let answer = crate::tui::modal::request_detail::answer_summary(entry);
        let answered = !entry.response.is_empty();
        let head = format!("{} {peer} ", clock(entry.unix_ms));
        let body = format!("{} → {answer}", entry.event_type);
        lines.push(InspectorLine::item(
            Item::Request(entry.id),
            vec![
                (head.clone(), Tone::Dim),
                (
                    fit(&body, width.saturating_sub(head.chars().count() + 2)),
                    if answered { Tone::Normal } else { Tone::Dim },
                ),
            ],
        ));
    }
    if shown == 0 {
        lines.push(InspectorLine::note("(no traffic yet)", Tone::Dim));
    }
    lines
}

fn rules_lines(instance: InstanceRef<'_>) -> Vec<InspectorLine> {
    use crate::scripting::event_handler::EventPattern;
    use crate::scripting::EventHandlerType;

    let mut lines = vec![InspectorLine::note(
        "Rules match in order; the first match answers. Enter edits a rule.",
        Tone::Dim,
    )];
    let mut has_wildcard = false;
    if let Some(config) = instance.routing() {
        for (index, handler) in config.handlers.iter().enumerate() {
            let pattern = match &handler.event_pattern {
                EventPattern::Specific(s) => s.clone(),
                EventPattern::Wildcard => {
                    has_wildcard = true;
                    "*".to_string()
                }
            };
            let (kind, tone, detail) = match &handler.handler {
                EventHandlerType::Llm { instruction } => (
                    "LLM",
                    Tone::Reasoning,
                    crate::utils::truncate_for_log(instruction, 48),
                ),
                EventHandlerType::Script {
                    language, resident, ..
                } => (
                    "SCRIPT",
                    Tone::Good,
                    format!("{language}{}", if *resident { ", resident" } else { "" }),
                ),
                EventHandlerType::Static { actions } => {
                    let names: Vec<&str> = actions
                        .iter()
                        .filter_map(|a| a.get("type").and_then(|t| t.as_str()))
                        .collect();
                    (
                        if actions.is_empty() {
                            "SILENT"
                        } else {
                            "STATIC"
                        },
                        if actions.is_empty() {
                            Tone::Dim
                        } else {
                            Tone::Good
                        },
                        if names.is_empty() {
                            "answer with nothing".to_string()
                        } else {
                            names.join(", ")
                        },
                    )
                }
                EventHandlerType::Manual { timeout_secs } => (
                    "MANUAL",
                    Tone::Warn,
                    format!("you answer · {timeout_secs}s before it fails closed"),
                ),
            };
            lines.push(InspectorLine::item(
                Item::Rule(index),
                vec![
                    (format!("{:>2}  ", index + 1), Tone::Dim),
                    (format!("{pattern} → "), Tone::Normal),
                    (kind.to_string(), tone),
                    (format!("  {detail}"), Tone::Dim),
                ],
            ));
        }
    }
    if !has_wildcard {
        lines.push(InspectorLine::note(
            "    otherwise → LLM, from the instance instruction",
            Tone::Dim,
        ));
    }
    lines
}

fn config_lines(instance: InstanceRef<'_>, width: usize) -> Vec<InspectorLine> {
    let mut lines = Vec::new();
    let value_width = width.saturating_sub(14);
    let push = |lines: &mut Vec<InspectorLine>, name: &str, value: String, tone: Tone| {
        lines.push(InspectorLine::item(
            Item::Config(name.to_string()),
            vec![
                (format!("{name:<12} "), Tone::Dim),
                (fit(&value, value_width), tone),
            ],
        ));
    };
    let (params, instruction, memory_len, tasks) = match instance {
        InstanceRef::Server(row) => {
            push(&mut lines, "protocol", row.protocol.clone(), Tone::Accent);
            push(
                &mut lines,
                "port",
                row.local_addr
                    .as_deref()
                    .and_then(|a| a.rsplit_once(':').map(|(_, p)| p.to_string()))
                    .unwrap_or_else(|| row.port.to_string()),
                Tone::Normal,
            );
            push(
                &mut lines,
                "host",
                row.local_addr
                    .as_deref()
                    .and_then(|a| a.rsplit_once(':').map(|(h, _)| h.to_string()))
                    .unwrap_or_else(|| "(default)".to_string()),
                Tone::Normal,
            );
            (
                row.startup_params.as_ref(),
                &row.instruction,
                row.memory_len,
                row.task_count,
            )
        }
        InstanceRef::Client(row) => {
            push(&mut lines, "protocol", row.protocol.clone(), Tone::Accent);
            push(&mut lines, "remote", row.remote_addr.clone(), Tone::Normal);
            (
                row.startup_params.as_ref(),
                &row.instruction,
                row.memory_len,
                row.task_count,
            )
        }
    };
    if let Some(map) = params.and_then(|p| p.as_object()) {
        for (k, v) in map {
            let shown = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            push(&mut lines, k, shown, Tone::Normal);
        }
    }
    let brief = instruction.trim();
    push(
        &mut lines,
        "instruction",
        if brief.is_empty() {
            "(none)".to_string()
        } else {
            brief.lines().next().unwrap_or("").to_string()
        },
        if brief.is_empty() {
            Tone::Dim
        } else {
            Tone::Normal
        },
    );
    push(
        &mut lines,
        "memory",
        format!("{memory_len} chars"),
        Tone::Dim,
    );
    if tasks > 0 {
        push(&mut lines, "tasks", format!("{tasks} scheduled"), Tone::Dim);
    }
    lines.push(InspectorLine::note(
        "Enter opens the form; changing port, host or remote restarts the instance.",
        Tone::Dim,
    ));
    lines
}

fn send_lines(instance: InstanceRef<'_>, width: usize) -> Vec<InspectorLine> {
    let InstanceRef::Client(row) = instance else {
        return vec![InspectorLine::note(
            "(servers answer; use [ message ] on a peer)",
            Tone::Dim,
        )];
    };
    let mut lines = Vec::new();
    match row.send_state {
        SendState::Ready => {}
        SendState::NotConnected => lines.push(InspectorLine::note(
            "(cannot send — not connected; [ connect ] redials)",
            Tone::Dim,
        )),
        SendState::ProtocolUnsupported => lines.push(InspectorLine::note(
            "(cannot send — this client's loop has no command channel yet)",
            Tone::Dim,
        )),
    }
    if row.send_actions.is_empty() {
        lines.push(InspectorLine::note(
            "(this protocol declares no client verbs)",
            Tone::Dim,
        ));
        return lines;
    }
    for (index, verb) in row.send_actions.iter().enumerate() {
        lines.push(InspectorLine::item(
            Item::SendAction(index),
            vec![
                (format!("{:<20} ", fit(&verb.name, 20)), Tone::Accent),
                (fit(&verb.description, width.saturating_sub(23)), Tone::Dim),
            ],
        ));
    }
    lines.push(InspectorLine::note(
        "Enter opens the composer on that verb's parameters.",
        Tone::Dim,
    ));
    lines
}

fn bar_buttons(
    instance: InstanceRef<'_>,
    tab: InspectorTab,
    selected: Option<&Item>,
    filter: TrafficFilter,
) -> Vec<BarButton> {
    let driver = driver_of(instance.routing());
    let driver_button = BarButton::on(
        InstanceAction::CycleDriver,
        format!("driver: {}", driver.label()),
    );
    match (tab, instance) {
        (InspectorTab::Overview, InstanceRef::Server(row)) => {
            let mut bar = vec![
                BarButton::on(InstanceAction::Stop, "stop"),
                BarButton::on(InstanceAction::Edit, "edit"),
                BarButton::on(InstanceAction::Rules, "rules"),
            ];
            bar.push(match &row.client_counterpart {
                Some(p) => BarButton::on(InstanceAction::ConnectClient, format!("+ {p} client")),
                None => BarButton::off(
                    InstanceAction::ConnectClient,
                    "+ client",
                    "no client implementation for this protocol is compiled in",
                ),
            });
            bar.push(driver_button);
            bar.push(BarButton::on(InstanceAction::Wireshark, "wireshark"));
            bar.push(BarButton::on(InstanceAction::Docs, "docs"));
            bar
        }
        (InspectorTab::Overview, InstanceRef::Client(row)) => {
            let mut bar = Vec::new();
            match row.send_state {
                SendState::NotConnected => {
                    bar.push(BarButton::on(InstanceAction::Connect, "connect"))
                }
                _ => bar.push(BarButton::on(InstanceAction::Disconnect, "disconnect")),
            }
            bar.push(BarButton::on(InstanceAction::Stop, "remove"));
            bar.push(match row.send_state {
                SendState::Ready if !row.send_actions.is_empty() => {
                    BarButton::on(InstanceAction::Send, "send…")
                }
                SendState::Ready => BarButton::off(
                    InstanceAction::Send,
                    "send…",
                    "this protocol declares no client verbs",
                ),
                SendState::NotConnected => {
                    BarButton::off(InstanceAction::Send, "send…", "not connected")
                }
                SendState::ProtocolUnsupported => BarButton::off(
                    InstanceAction::Send,
                    "send…",
                    "this client's loop has no command channel yet",
                ),
            });
            bar.push(BarButton::on(InstanceAction::Edit, "edit"));
            bar.push(BarButton::on(InstanceAction::Rules, "rules"));
            bar.push(driver_button);
            bar.push(BarButton::on(InstanceAction::Wireshark, "wireshark"));
            bar
        }
        (InspectorTab::Peers, InstanceRef::Server(row)) => {
            let peer = match selected {
                Some(Item::Peer(Some(id))) => row.conns.iter().find(|c| c.id == *id && c.active),
                _ => None,
            };
            let mut bar = Vec::new();
            match peer {
                Some(conn) if conn.can_message => {
                    bar.push(BarButton::on(
                        InstanceAction::MessagePeer(conn.id),
                        "message",
                    ));
                    bar.push(BarButton::on(
                        InstanceAction::DisconnectPeer(conn.id),
                        "disconnect",
                    ));
                }
                Some(conn) => {
                    let why = "this protocol cannot message or disconnect a peer from here yet";
                    bar.push(BarButton::off(
                        InstanceAction::MessagePeer(conn.id),
                        "message",
                        why,
                    ));
                    bar.push(BarButton::off(
                        InstanceAction::DisconnectPeer(conn.id),
                        "disconnect",
                        why,
                    ));
                }
                None => {
                    let why = "select a live peer first";
                    bar.push(BarButton::off(
                        InstanceAction::MessagePeer(0),
                        "message",
                        why,
                    ));
                    bar.push(BarButton::off(
                        InstanceAction::DisconnectPeer(0),
                        "disconnect",
                        why,
                    ));
                }
            }
            bar.push(match &row.client_counterpart {
                Some(p) => BarButton::on(InstanceAction::ConnectClient, format!("+ {p} client")),
                None => BarButton::off(
                    InstanceAction::ConnectClient,
                    "+ client",
                    "no client implementation for this protocol is compiled in",
                ),
            });
            bar
        }
        (InspectorTab::Peers, InstanceRef::Client(row)) => match row.send_state {
            SendState::NotConnected => vec![BarButton::on(InstanceAction::Connect, "connect")],
            _ => vec![BarButton::on(InstanceAction::Disconnect, "disconnect")],
        },
        (InspectorTab::Traffic, _) => {
            let mut bar = vec![match selected {
                Some(Item::Request(id)) => BarButton::on(InstanceAction::OpenRequest(*id), "open"),
                _ => BarButton::off(InstanceAction::OpenRequest(0), "open", "select a request"),
            }];
            if filter != TrafficFilter::All {
                bar.push(BarButton::on(
                    InstanceAction::ClearTrafficFilter,
                    "all peers",
                ));
            }
            bar
        }
        (InspectorTab::Rules, _) => {
            let count = instance.routing().map(|c| c.handlers.len()).unwrap_or(0);
            let mut bar = vec![BarButton::on(InstanceAction::AddRule, "+ add")];
            match selected {
                Some(Item::Rule(index)) => {
                    bar.push(BarButton::on(InstanceAction::EditRule(*index), "edit"));
                    bar.push(BarButton::on(InstanceAction::DeleteRule(*index), "delete"));
                    if count > 1 {
                        bar.push(BarButton::on(InstanceAction::MoveRule(*index, -1), "up"));
                        bar.push(BarButton::on(InstanceAction::MoveRule(*index, 1), "down"));
                    }
                }
                _ => {
                    bar.push(BarButton::off(
                        InstanceAction::EditRule(0),
                        "edit",
                        "select a rule",
                    ));
                    bar.push(BarButton::off(
                        InstanceAction::DeleteRule(0),
                        "delete",
                        "select a rule",
                    ));
                }
            }
            bar.push(driver_button);
            bar
        }
        (InspectorTab::Config, _) => vec![
            BarButton::on(InstanceAction::Edit, "edit"),
            BarButton::on(InstanceAction::Wireshark, "wireshark"),
        ],
        (InspectorTab::Send, InstanceRef::Client(row)) => {
            let mut bar = Vec::new();
            match selected {
                Some(Item::SendAction(index)) if row.send_state == SendState::Ready => {
                    bar.push(BarButton::on(InstanceAction::SendAction(*index), "compose"));
                }
                _ => {}
            }
            bar.push(match row.send_state {
                SendState::Ready if !row.send_actions.is_empty() => {
                    BarButton::on(InstanceAction::Send, "pick a verb…")
                }
                SendState::Ready => BarButton::off(InstanceAction::Send, "send…", "no verbs"),
                SendState::NotConnected => BarButton::on(InstanceAction::Connect, "connect"),
                SendState::ProtocolUnsupported => BarButton::off(
                    InstanceAction::Send,
                    "send…",
                    "this client's loop has no command channel yet",
                ),
            });
            bar
        }
        (InspectorTab::Send, InstanceRef::Server(_)) => Vec::new(),
    }
}
