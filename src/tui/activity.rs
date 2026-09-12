//! The activity feed: what the machine is doing, as it happens.
//!
//! Two sources. The status channel's `[LEVEL]` lines arrive as log entries
//! and are filtered by the log level at render time. Everything structural —
//! an instance starting, a peer connecting, a request being answered, a
//! question parked for the human — is derived by [`Tracker::diff`] from the
//! difference between two successive snapshots, so it is exact rather than
//! parsed out of log text, and each entry can carry a link back to the thing
//! it describes (Enter opens it).

use std::collections::{HashMap, HashSet, VecDeque};

use crate::state::{ClientId, ServerId};
use crate::tui::app::UiKey;
use crate::tui::chat::ScrollPos;
use crate::tui::metrics::{clock, clock_now, human_bytes};
use crate::tui::projection::RailSnapshot;
use crate::ui::app::LogLevel;

pub const ACTIVITY_CAPACITY: usize = 5_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// A `[LEVEL]` status line.
    Log(LogLevel),
    /// An instance started, changed status or stopped.
    Lifecycle,
    /// A peer connected or closed.
    Peer,
    /// A request arrived and was answered (or not).
    Request,
    /// A request is parked for the human.
    Waiting,
    /// A lifecycle event that went wrong (bind failed, connect refused).
    Failure,
}

/// What an entry points at, for Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Request(UiKey, u64),
    Intercept(UiKey, u64),
    Instance(UiKey),
}

/// An event before it is sequenced into the feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub kind: ActivityKind,
    pub owner: Option<UiKey>,
    /// `http#1`, `telnet#4`; empty for global lines.
    pub tag: String,
    pub text: String,
    pub link: Option<Link>,
    /// Unix ms the event happened at, when known; else "now".
    pub at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ActivityEntry {
    pub seq: u64,
    pub time: String,
    pub event: Activity,
}

pub struct ActivityFeed {
    pub entries: VecDeque<ActivityEntry>,
    pub scroll: ScrollPos,
    /// Entries that arrived while scrolled away from the tail.
    pub unseen: usize,
    /// Cursor into the *visible* entries while the feed has focus.
    pub cursor: Option<usize>,
    /// Show only the selected instance's entries (and global ones).
    pub only_selected: bool,
    next_seq: u64,
}

impl Default for ActivityFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityFeed {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            scroll: ScrollPos::Follow,
            unseen: 0,
            cursor: None,
            only_selected: false,
            next_seq: 1,
        }
    }

    pub fn push(&mut self, event: Activity) {
        let time = event.at_unix_ms.map(clock).unwrap_or_else(clock_now);
        self.entries.push_back(ActivityEntry {
            seq: self.next_seq,
            time,
            event,
        });
        self.next_seq += 1;
        if self.entries.len() > ACTIVITY_CAPACITY {
            self.entries.pop_front();
        }
        if self.scroll != ScrollPos::Follow {
            self.unseen += 1;
        }
    }

    pub fn push_log(&mut self, level: LogLevel, text: String) {
        self.push(Activity {
            kind: ActivityKind::Log(level),
            owner: None,
            tag: String::new(),
            text,
            link: None,
            at_unix_ms: None,
        });
    }

    /// Whether an entry is shown under the current log level and instance
    /// filter. Structural entries always pass the level filter — they are the
    /// point of the feed — and global log lines always pass the instance one.
    pub fn passes(entry: &ActivityEntry, level: LogLevel, only: Option<UiKey>) -> bool {
        if let ActivityKind::Log(entry_level) = entry.event.kind {
            if entry_level > level {
                return false;
            }
        }
        match (only, entry.event.owner) {
            (Some(wanted), Some(owner)) => wanted == owner,
            _ => true,
        }
    }

    pub fn scroll_to_follow(&mut self) {
        self.scroll = ScrollPos::Follow;
        self.unseen = 0;
        self.cursor = None;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = match self.scroll {
            ScrollPos::Follow => ScrollPos::Up(lines),
            ScrollPos::Up(n) => ScrollPos::Up(n.saturating_add(lines)),
        };
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = match self.scroll {
            ScrollPos::Follow => ScrollPos::Follow,
            ScrollPos::Up(n) => {
                let n = n.saturating_sub(lines);
                if n == 0 {
                    self.unseen = 0;
                    ScrollPos::Follow
                } else {
                    ScrollPos::Up(n)
                }
            }
        };
    }
}

#[derive(Debug, Clone)]
struct ServerSeen {
    status: String,
    live: HashMap<u32, String>,
}

#[derive(Debug, Clone)]
struct ClientSeen {
    status: String,
    connected: bool,
}

/// Remembers the last snapshot well enough to say what changed.
#[derive(Debug, Default)]
pub struct Tracker {
    seeded: bool,
    servers: HashMap<ServerId, ServerSeen>,
    clients: HashMap<ClientId, ClientSeen>,
    last_request: u64,
    intercepts: HashSet<u64>,
}

fn server_tag(row: &crate::tui::projection::ServerRow) -> String {
    format!("{}#{}", row.protocol.to_lowercase(), row.id.as_u32())
}

fn client_tag(row: &crate::tui::projection::ClientRow) -> String {
    format!("{}#{}", row.protocol.to_lowercase(), row.id.as_u32())
}

fn server_addr(row: &crate::tui::projection::ServerRow) -> String {
    row.local_addr
        .clone()
        .unwrap_or_else(|| format!(":{}", row.port))
}

impl Tracker {
    /// Everything that changed between the previous snapshot and this one.
    ///
    /// The first call only seeds: instances present at startup (a `--load`)
    /// are already in the list, and their backlog of requests is history, not
    /// activity.
    pub fn diff(&mut self, snapshot: &RailSnapshot) -> Vec<Activity> {
        let mut out = Vec::new();
        let emit = self.seeded;

        // ---- servers ----
        let mut seen_servers: HashSet<ServerId> = HashSet::new();
        for row in &snapshot.servers {
            seen_servers.insert(row.id);
            let key = UiKey::Server(row.id);
            let tag = server_tag(row);
            let status = row.status.to_string();
            let live: HashMap<u32, String> = row
                .conns
                .iter()
                .filter(|c| c.active)
                .map(|c| (c.id, c.remote_addr.clone()))
                .collect();

            match self.servers.get(&row.id) {
                None => {
                    if emit {
                        let (kind, text) = lifecycle_text(&row.status, &server_addr(row));
                        out.push(Activity {
                            kind,
                            owner: Some(key),
                            tag: tag.clone(),
                            text,
                            link: Some(Link::Instance(key)),
                            at_unix_ms: None,
                        });
                        // Peers it already has are new to the feed.
                        for addr in live.values() {
                            out.push(Activity {
                                kind: ActivityKind::Peer,
                                owner: Some(key),
                                tag: tag.clone(),
                                text: format!("⇐ {addr} connected"),
                                link: Some(Link::Instance(key)),
                                at_unix_ms: None,
                            });
                        }
                    }
                }
                Some(previous) => {
                    if previous.status != status {
                        let (kind, text) = lifecycle_text(&row.status, &server_addr(row));
                        out.push(Activity {
                            kind,
                            owner: Some(key),
                            tag: tag.clone(),
                            text,
                            link: Some(Link::Instance(key)),
                            at_unix_ms: None,
                        });
                    }
                    for (id, addr) in &live {
                        if !previous.live.contains_key(id) {
                            out.push(Activity {
                                kind: ActivityKind::Peer,
                                owner: Some(key),
                                tag: tag.clone(),
                                text: format!("⇐ {addr} connected"),
                                link: Some(Link::Instance(key)),
                                at_unix_ms: None,
                            });
                        }
                    }
                    for (id, addr) in &previous.live {
                        if !live.contains_key(id) {
                            let (rx, tx) = row
                                .conns
                                .iter()
                                .find(|c| c.id == *id)
                                .map(|c| (c.bytes_received, c.bytes_sent))
                                .or_else(|| {
                                    row.recent
                                        .iter()
                                        .find(|c| c.id == *id)
                                        .map(|c| (c.bytes_received, c.bytes_sent))
                                })
                                .unwrap_or((0, 0));
                            out.push(Activity {
                                kind: ActivityKind::Peer,
                                owner: Some(key),
                                tag: tag.clone(),
                                text: format!(
                                    "{addr} closed · ↓{} ↑{}",
                                    human_bytes(rx),
                                    human_bytes(tx)
                                ),
                                link: Some(Link::Instance(key)),
                                at_unix_ms: None,
                            });
                        }
                    }
                }
            }
            self.servers.insert(row.id, ServerSeen { status, live });
        }
        let gone: Vec<ServerId> = self
            .servers
            .keys()
            .filter(|id| !seen_servers.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.servers.remove(&id);
            if emit {
                out.push(Activity {
                    kind: ActivityKind::Lifecycle,
                    owner: None,
                    tag: format!("#{}", id.as_u32()),
                    text: "server stopped and removed".to_string(),
                    link: None,
                    at_unix_ms: None,
                });
            }
        }

        // ---- clients ----
        let mut seen_clients: HashSet<ClientId> = HashSet::new();
        for row in &snapshot.clients {
            seen_clients.insert(row.id);
            let key = UiKey::Client(row.id);
            let tag = client_tag(row);
            let status = row.status.to_string();
            let connected = row.status == crate::state::client::ClientStatus::Connected;
            let changed = match self.clients.get(&row.id) {
                None => emit,
                Some(previous) => previous.status != status || previous.connected != connected,
            };
            if changed {
                let (kind, text) = client_text(&row.status, &row.remote_addr);
                out.push(Activity {
                    kind,
                    owner: Some(key),
                    tag,
                    text,
                    link: Some(Link::Instance(key)),
                    at_unix_ms: None,
                });
            }
            self.clients
                .insert(row.id, ClientSeen { status, connected });
        }
        let gone: Vec<ClientId> = self
            .clients
            .keys()
            .filter(|id| !seen_clients.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.clients.remove(&id);
            if emit {
                out.push(Activity {
                    kind: ActivityKind::Lifecycle,
                    owner: None,
                    tag: format!("#{}", id.as_u32()),
                    text: "client removed".to_string(),
                    link: None,
                    at_unix_ms: None,
                });
            }
        }

        // ---- requests: every access-log entry newer than the last seen ----
        let mut fresh: Vec<(u64, Activity)> = Vec::new();
        for row in &snapshot.servers {
            let key = UiKey::Server(row.id);
            let tag = server_tag(row);
            for entry in &row.requests {
                if entry.id > self.last_request {
                    let peer = entry
                        .connection_id
                        .and_then(|c| row.conns.iter().find(|x| x.id == c))
                        .map(|c| c.remote_addr.clone())
                        .or_else(|| {
                            entry
                                .connection_id
                                .and_then(|c| row.recent.iter().find(|x| x.id == c))
                                .map(|c| c.remote_addr.clone())
                        });
                    fresh.push((entry.id, request_activity(key, &tag, peer, entry)));
                }
            }
        }
        for row in &snapshot.clients {
            let key = UiKey::Client(row.id);
            let tag = client_tag(row);
            for entry in &row.requests {
                if entry.id > self.last_request {
                    fresh.push((entry.id, request_activity(key, &tag, None, entry)));
                }
            }
        }
        fresh.sort_by_key(|(id, _)| *id);
        if let Some((max, _)) = fresh.last() {
            self.last_request = *max;
        }
        if emit {
            out.extend(fresh.into_iter().map(|(_, a)| a));
        }

        // ---- intercepts ----
        let mut current: HashSet<u64> = HashSet::new();
        let mut waiting: Vec<(u64, Activity)> = Vec::new();
        for row in &snapshot.servers {
            let key = UiKey::Server(row.id);
            for view in &row.intercepts {
                current.insert(view.id);
                if !self.intercepts.contains(&view.id) {
                    waiting.push((
                        view.id,
                        waiting_activity(key, &server_tag(row), view, {
                            view.connection_id
                                .and_then(|c| row.conns.iter().find(|x| x.id == c))
                                .map(|c| c.remote_addr.clone())
                        }),
                    ));
                }
            }
        }
        for row in &snapshot.clients {
            let key = UiKey::Client(row.id);
            for view in &row.intercepts {
                current.insert(view.id);
                if !self.intercepts.contains(&view.id) {
                    waiting.push((view.id, waiting_activity(key, &client_tag(row), view, None)));
                }
            }
        }
        waiting.sort_by_key(|(id, _)| *id);
        self.intercepts = current;
        if emit {
            out.extend(waiting.into_iter().map(|(_, a)| a));
        }

        self.seeded = true;
        out
    }
}

/// `:53121` for `127.0.0.1:53121` — the port is what tells peers apart.
fn port_of(addr: &str) -> String {
    addr.rsplit_once(':')
        .map(|(_, p)| format!(":{p}"))
        .unwrap_or_else(|| addr.to_string())
}

fn lifecycle_text(
    status: &crate::state::server::ServerStatus,
    addr: &str,
) -> (ActivityKind, String) {
    use crate::state::server::ServerStatus;
    match status {
        ServerStatus::Starting => (ActivityKind::Lifecycle, format!("starting on {addr}")),
        ServerStatus::Running => (ActivityKind::Lifecycle, format!("listening on {addr}")),
        ServerStatus::Stopped => (ActivityKind::Lifecycle, "stopped".to_string()),
        ServerStatus::Error(e) => (ActivityKind::Failure, e.clone()),
    }
}

fn client_text(
    status: &crate::state::client::ClientStatus,
    remote: &str,
) -> (ActivityKind, String) {
    use crate::state::client::ClientStatus;
    match status {
        ClientStatus::Connecting => (ActivityKind::Lifecycle, format!("connecting to {remote}")),
        ClientStatus::Connected => (ActivityKind::Lifecycle, format!("connected to {remote}")),
        ClientStatus::Disconnected => (
            ActivityKind::Lifecycle,
            format!("disconnected from {remote}"),
        ),
        ClientStatus::Error(e) => (ActivityKind::Failure, e.clone()),
    }
}

fn request_activity(
    key: UiKey,
    tag: &str,
    peer: Option<String>,
    entry: &crate::state::app_state::AccessLogEntry,
) -> Activity {
    let answer = crate::tui::modal::request_detail::answer_summary(entry);
    let text = match peer.as_deref().map(port_of) {
        Some(peer) => format!("{peer} {} → {answer}", entry.event_type),
        None => format!("{} → {answer}", entry.event_type),
    };
    Activity {
        kind: ActivityKind::Request,
        owner: Some(key),
        tag: tag.to_string(),
        text,
        link: Some(Link::Request(key, entry.id)),
        at_unix_ms: Some(entry.unix_ms),
    }
}

fn waiting_activity(
    key: UiKey,
    tag: &str,
    view: &crate::state::intercepts::InterceptView,
    peer: Option<String>,
) -> Activity {
    let text = match peer.as_deref().map(port_of) {
        Some(peer) => format!("{} from {peer} needs YOUR answer", view.event_type),
        None => format!("{} needs YOUR answer", view.event_type),
    };
    Activity {
        kind: ActivityKind::Waiting,
        owner: Some(key),
        tag: tag.to_string(),
        text,
        link: Some(Link::Intercept(key, view.id)),
        at_unix_ms: Some(view.created_unix_ms),
    }
}
