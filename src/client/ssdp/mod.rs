//! SSDP / UPnP discovery client — socket loop.
//!
//! A control point: it sends `M-SEARCH * HTTP/1.1` and reads what answers. Pointed at a real
//! LAN this enumerates actual hardware — routers, TVs, printers, media servers — and the
//! model decides from what it finds what to search for next.
//!
//! # The shape that makes this different from every other client here
//!
//! **SSDP is one-to-many.** Every other client in this tree issues a request and reads *the*
//! response; SSDP issues one datagram and an unknown number of devices answer over the next
//! `MX` seconds, each from its own address. Returning on the first datagram would report one
//! device on a network of thirty. So a search opens a *collection window*: each responder
//! raises its own `ssdp_search_response` event as it arrives, and when the window closes a
//! single `ssdp_search_complete` says how many answered and who they were.
//!
//! # One task owns the search
//!
//! Three things need to interleave — datagrams arriving, the collection window expiring, and
//! injected commands from the dashboard — and all three mutate the same search state. Rather
//! than share it behind a mutex, one task ([`Session::run`]) owns it outright and selects
//! over all three sources. A separate, minimal receive task does nothing but `recv_from` and
//! forward, so the socket is always ready even while the session is inside an LLM call and no
//! datagram is dropped for want of a reader.
//!
//! The cost is that an injected `[ send ]` waits behind an in-flight LLM call rather than
//! running concurrently (`udp` gives commands their own task for exactly that reason). That
//! is the right trade here: a command that started a second search from another task would
//! race the window bookkeeping, and the command channel is bounded, so the command queues
//! rather than being lost.
//!
//! # Iterative discovery is bounded, and the bound is loud
//!
//! Discovery is inherently recursive: search, see what answers, search again more
//! specifically. Nothing in that shape converges on its own, so a search started in reply to
//! an event carries a depth, and [`MAX_FOLLOWUP_DEPTH`] refuses to go deeper — with an ERROR
//! on both log channels naming the limit, never silently.
//!
//! No `Box::pin` is needed for it, and that is worth saying because the usual fix here is a
//! boxed recursive call. The cycle already passes through a queue: `send_msearch` only
//! *enqueues* a search, the session loop starts it, the socket delivers the answers later,
//! and the events those raise are handled on a subsequent turn of the same loop. There is no
//! `async fn` that awaits itself, so there is no infinitely-sized future to box — the same
//! situation the root `CLAUDE.md` describes for `datalink`'s pcap loop.

pub mod actions;

pub use actions::SsdpClientProtocol;

use anyhow::{Context, Result};
use std::collections::{HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::ssdp::actions::{
    SSDP_CLIENT_CONNECTED_EVENT, SSDP_NOTIFY_RECEIVED_EVENT, SSDP_SEARCH_COMPLETE_EVENT,
    SSDP_SEARCH_RESPONSE_EVENT,
};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::{Event, StartupParams};
use crate::server::ssdp::message::{self, HttpuMessage, SSDP_GROUP_V4, SSDP_PORT};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// How many searches deep a chain started by an event may go.
///
/// Chain: connected event → search (depth 0) → its responses/completion (depth 1) → a
/// narrower search (depth 1) → … A model that answers every `ssdp_search_complete` with
/// another search would otherwise sweep the network forever, and the per-client LLM budget
/// (100 calls) is a blunt backstop rather than a bound on *this*.
///
/// Six allows the search-narrow-narrow pattern a real enumeration needs with room to spare,
/// and stops well short of the budget so exhaustion is reported as what it is.
pub const MAX_FOLLOWUP_DEPTH: usize = 6;

/// Default `USER-AGENT` when the operator does not set one.
const DEFAULT_USER_AGENT: &str = "NetGet/1.0 UPnP/1.1 NetGet-SSDP-Client/1.0";

/// A datagram handed from the receive task to the session task.
struct Inbound {
    data: Vec<u8>,
    source: SocketAddr,
}

/// A search that has been asked for but not yet put on the wire.
#[derive(Clone, Debug)]
struct PendingSearch {
    st: String,
    mx: u32,
    target: SocketAddr,
    /// Depth of the *event* that asked for this search; see [`MAX_FOLLOWUP_DEPTH`].
    depth: usize,
}

/// A search whose collection window is open.
struct ActiveSearch {
    st: String,
    mx: u32,
    target: SocketAddr,
    depth: usize,
    /// When the window closes and `ssdp_search_complete` fires.
    deadline: Instant,
    /// One entry per distinct responder, in arrival order.
    responders: Vec<serde_json::Value>,
    /// `(source address, USN)` of everything already reported. A device that retransmits
    /// its answer is one device, not two — but a device answering `ssdp:all` with several
    /// *different* USNs is genuinely several services, and the pair keeps those apart.
    seen: HashSet<(String, String)>,
    duplicates: usize,
}

/// SSDP discovery client.
pub struct SsdpClient;

impl SsdpClient {
    /// Bind the search socket and run discovery under LLM control.
    ///
    /// Returns as soon as the socket is bound and the loops are spawned: the
    /// `ssdp_connected` LLM call happens inside the session task, so a `manual` routing rule
    /// that parks it for a human does not hold up client creation.
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        let params = startup_params.as_ref();

        let bind_address = match params {
            Some(p) => p
                .get_optional_string("bind_address")?
                .unwrap_or_else(|| "0.0.0.0".to_string()),
            None => "0.0.0.0".to_string(),
        };
        let local_port = match params {
            Some(p) => p.get_optional_u64("local_port")?.unwrap_or(0),
            None => 0,
        };
        let join_multicast = match params {
            Some(p) => p.get_optional_bool("join_multicast")?.unwrap_or(true),
            None => true,
        };
        let multicast_interface = match params {
            Some(p) => p
                .get_optional_string("multicast_interface")?
                .unwrap_or_else(|| "0.0.0.0".to_string()),
            None => "0.0.0.0".to_string(),
        };
        let response_window_ms = match params {
            Some(p) => p.get_optional_u64("response_window_ms")?.unwrap_or(0),
            None => 0,
        };
        let user_agent = match params {
            Some(p) => p
                .get_optional_string("user_agent")?
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            None => DEFAULT_USER_AGENT.to_string(),
        };

        let local_port = u16::try_from(local_port)
            .map_err(|_| anyhow::anyhow!("local_port must be 0..=65535, got {local_port}"))?;

        // Where searches go unless an action overrides it. The multicast group is the
        // default a real scan wants; a unicast address searches exactly one device.
        let default_target = resolve_target(&remote_addr).await?;

        // Bind first and propagate the error: a client reported Connected on a socket that
        // never bound is the ARP/DataLink defect wearing a different hat.
        let socket = UdpSocket::bind(format!("{bind_address}:{local_port}"))
            .await
            .with_context(|| {
                format!(
                    "Failed to bind SSDP search socket to {bind_address}:{local_port}. \
                     Port 1900 is often already held by a system UPnP service; use \
                     local_port 0 to search from an ephemeral port instead (NOTIFY \
                     announcements will then not be seen)."
                )
            })?;
        let local_addr = socket.local_addr()?;

        // Best effort, exactly like the server half. Searching does not depend on the join —
        // replies to our own M-SEARCH come back unicast — so refusing to start here would
        // make the client unusable for the local testing it is most used for. Logged rather
        // than passed over, because a silent failure presents as "no device ever announces
        // itself", which is indistinguishable from a broken client.
        let mut multicast_joined = false;
        if join_multicast {
            match multicast_interface.parse::<Ipv4Addr>() {
                Ok(iface) => match socket.join_multicast_v4(SSDP_GROUP_V4, iface) {
                    Ok(()) => {
                        multicast_joined = true;
                        info!(
                            "SSDP client {} joined {} on interface {}",
                            client_id, SSDP_GROUP_V4, iface
                        );
                        if local_port != SSDP_PORT {
                            // Announcements are multicast to the group on the well-known
                            // port; a socket on an ephemeral port is in the group but will
                            // never be delivered any.
                            warn!(
                                "SSDP client {} joined the group but is bound to port {} \
                                 rather than {}, so NOTIFY announcements will not arrive. \
                                 Set startup param local_port to {} to overhear them.",
                                client_id, local_port, SSDP_PORT, SSDP_PORT
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "SSDP client {} could not join {}: {}. Unicast searches still \
                             work; unsolicited NOTIFY announcements will not be seen.",
                            client_id, SSDP_GROUP_V4, e
                        );
                        let _ = status_tx.send(format!(
                            "[CLIENT] ⚠ SSDP client {} could not join the multicast group: {}",
                            client_id, e
                        ));
                    }
                },
                Err(e) => {
                    warn!(
                        "SSDP client {} ignoring multicast_interface '{}': not an IPv4 \
                         address ({})",
                        client_id, multicast_interface, e
                    );
                }
            }
        }

        info!(
            "SSDP client {} bound to {} (default search target: {}, multicast joined: {})",
            client_id, local_addr, default_target, multicast_joined
        );

        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        // `ClientInstance::connection` (and its byte counters) is never constructed anywhere
        // in this tree, so there is no per-client stats plumbing to feed. `protocol_data` is
        // the mechanism that does exist and that the dashboard reads.
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "ssdp_local_addr".to_string(),
                    serde_json::json!(local_addr.to_string()),
                );
                client.set_protocol_field(
                    "ssdp_default_target".to_string(),
                    serde_json::json!(default_target.to_string()),
                );
                client.set_protocol_field(
                    "ssdp_multicast_joined".to_string(),
                    serde_json::json!(multicast_joined),
                );
            })
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] SSDP client {} ready on {} (searching {})",
            client_id, local_addr, default_target
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Registered BEFORE anything that can park: a dashboard-created client defaults to a
        // `*` -> manual rule, so the connected event can wait on a human for minutes, and
        // `[ send ]` must work for the whole park rather than reading "no command channel".
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        let socket = Arc::new(socket);
        let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<Inbound>();

        // Receive task: nothing but recv_from and forward. Kept free of LLM calls on purpose
        // — the session task is inside a model round-trip for most of a busy window, and a
        // reader that waited on it would leave datagrams in the kernel buffer to be dropped.
        let recv_socket = socket.clone();
        let recv_state = app_state.clone();
        let recv_status = status_tx.clone();
        let recv_task = tokio::spawn(async move {
            // MAX_MESSAGE_LEN is the largest message the codec will parse; one byte more
            // lets an over-long datagram be *detected* rather than silently truncated into
            // plausible-looking headers.
            let mut buffer = vec![0u8; message::MAX_MESSAGE_LEN + 1];
            loop {
                match recv_socket.recv_from(&mut buffer).await {
                    Ok((n, source)) => {
                        trace!(
                            "SSDP client {} received {} bytes from {}",
                            client_id,
                            n,
                            source
                        );
                        if inbound_tx
                            .send(Inbound {
                                data: buffer[..n].to_vec(),
                                source,
                            })
                            .is_err()
                        {
                            // The session task ended; nothing left to hand datagrams to.
                            break;
                        }
                    }
                    Err(e) => {
                        error!("SSDP client {} receive error: {}", client_id, e);
                        recv_state
                            .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                            .await;
                        let _ = recv_status.send(format!(
                            "[CLIENT] ✖ SSDP client {} receive error: {}",
                            client_id, e
                        ));
                        let _ = recv_status.send("__UPDATE_UI__".to_string());
                        break;
                    }
                }
            }
        });
        app_state.register_client_task(client_id, recv_task).await;

        let session = Session {
            client_id,
            socket: socket.clone(),
            app_state: app_state.clone(),
            llm_client,
            status_tx: status_tx.clone(),
            protocol: SsdpClientProtocol::new(),
            default_target,
            local_addr,
            multicast_joined,
            response_window_ms,
            user_agent,
            memory: String::new(),
            active: None,
            queue: VecDeque::new(),
            done: false,
        };

        let session_task = tokio::spawn(async move {
            session.run(inbound_rx, command_rx).await;
        });
        app_state
            .register_client_task(client_id, session_task)
            .await;

        Ok(local_addr)
    }
}

/// Resolve the client's `remote_addr` into the default search destination.
///
/// A literal `ip:port` is used as-is; anything else goes through the resolver, so
/// `router.local:1900` works. The default for a real scan is the multicast group
/// `239.255.255.250:1900`.
async fn resolve_target(remote_addr: &str) -> Result<SocketAddr> {
    if let Ok(addr) = remote_addr.parse::<SocketAddr>() {
        return Ok(addr);
    }
    tokio::net::lookup_host(remote_addr)
        .await
        .with_context(|| format!("Failed to resolve SSDP search target '{remote_addr}'"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("'{remote_addr}' resolved to no addresses"))
}

/// What applying one action did. Shared by the LLM path and the command channel so an
/// injected `[ send ]` and a model-chosen action cannot diverge.
enum Applied {
    /// A search was queued. It reaches the wire on the next turn of the session loop, so no
    /// byte count exists yet and reporting one would be inventing it.
    Queued {
        st: String,
    },
    Waited,
    Disconnect,
    Refused(String),
}

/// Everything one discovery session owns. Single-task by construction: no mutex, because
/// nothing else touches it.
struct Session {
    client_id: ClientId,
    socket: Arc<UdpSocket>,
    app_state: Arc<AppState>,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: SsdpClientProtocol,
    default_target: SocketAddr,
    local_addr: SocketAddr,
    multicast_joined: bool,
    response_window_ms: u64,
    user_agent: String,
    memory: String,
    active: Option<ActiveSearch>,
    queue: VecDeque<PendingSearch>,
    done: bool,
}

impl Session {
    async fn run(
        mut self,
        mut inbound_rx: mpsc::UnboundedReceiver<Inbound>,
        mut command_rx: mpsc::Receiver<ClientCommand>,
    ) {
        // The session opens with the connected event: nothing is on the wire yet, and the
        // model's answer is what decides the first search.
        let event = Event::new(
            &SSDP_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "local_addr": self.local_addr.to_string(),
                "default_target": self.default_target.to_string(),
                "multicast_joined": self.multicast_joined,
            }),
        );
        // Bound before the `if let`, deliberately: a temporary in an `if let` scrutinee lives
        // for the whole block, so `if let Some(a) = self.call_model(..).await` keeps the
        // `&mut self` borrow alive across `self.apply_actions(..)` inside it and does not
        // compile. Every call site here has the same shape for the same reason.
        let actions = self.call_model(&event).await;
        if let Some(actions) = actions {
            self.apply_actions(actions, 0).await;
        }

        while !self.done {
            // Start whatever the last turn asked for. A search is only ever *enqueued* by an
            // action; it is started here, which is what keeps the search → event → search
            // cycle from being a recursive call.
            self.drain_queue().await;

            let deadline = self.active.as_ref().map(|s| s.deadline);

            tokio::select! {
                biased;

                inbound = inbound_rx.recv() => {
                    match inbound {
                        Some(inbound) => self.handle_inbound(inbound).await,
                        None => {
                            debug!(
                                "SSDP client {} receive loop ended; session closing",
                                self.client_id
                            );
                            break;
                        }
                    }
                }

                command = command_rx.recv() => {
                    match command {
                        Some(command) => self.handle_command(command).await,
                        None => break,
                    }
                }

                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                    if deadline.is_some() =>
                {
                    self.finish_search().await;
                }
            }
        }

        self.app_state
            .update_client_status(self.client_id, ClientStatus::Disconnected)
            .await;
        // Every exit lands here: drop the command handle so the dashboard stops offering
        // [ send ] on a dead client and a late send fails fast instead of timing out.
        self.app_state.remove_client_handle(self.client_id).await;
        let _ = self.status_tx.send(format!(
            "[CLIENT] SSDP client {} discovery finished",
            self.client_id
        ));
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
    }

    // -----------------------------------------------------------------------
    // Searching
    // -----------------------------------------------------------------------

    /// Start queued searches while nothing is in flight.
    ///
    /// A send failure is reported to the **model**, not only to the log: it raises
    /// `ssdp_search_complete` with `send_error` set and no responders, so the model can react
    /// — which matters most for the one failure that actually happens here, sending to the
    /// multicast group from a loopback-bound socket (`EADDRNOTAVAIL`, because loopback
    /// carries no multicast route). The answer to that is a unicast `target`, and only the
    /// model can choose one.
    async fn drain_queue(&mut self) {
        while self.active.is_none() {
            let Some(pending) = self.queue.pop_front() else {
                break;
            };
            match self.send_search(&pending).await {
                Ok(bytes) => {
                    info!(
                        "SSDP client {} sent M-SEARCH ST={} MX={} to {} ({} bytes)",
                        self.client_id, pending.st, pending.mx, pending.target, bytes
                    );
                    let _ = self.status_tx.send(format!(
                        "[CLIENT] SSDP client {} searching for {} (MX {})",
                        self.client_id, pending.st, pending.mx
                    ));
                    let _ = self.status_tx.send("__UPDATE_UI__".to_string());
                }
                Err(e) => {
                    let detail = send_failure_hint(&e, pending.target, self.local_addr);
                    error!(
                        "SSDP client {} could not send M-SEARCH ST={} to {}: {}",
                        self.client_id, pending.st, pending.target, detail
                    );
                    let _ = self.status_tx.send(format!(
                        "[CLIENT] ✖ SSDP client {} M-SEARCH to {} failed: {}",
                        self.client_id, pending.target, detail
                    ));
                    self.complete_search(&pending, Vec::new(), 0, Some(detail))
                        .await;
                }
            }
        }
    }

    /// Render and send one M-SEARCH, then open its collection window.
    async fn send_search(&mut self, pending: &PendingSearch) -> Result<usize> {
        let datagram = render_msearch(&pending.st, pending.mx, pending.target, &self.user_agent);
        let bytes = self
            .socket
            .send_to(datagram.as_bytes(), pending.target)
            .await?;

        // The window is what the devices were told to expect: MX seconds, unless the
        // operator overrode it. A window shorter than MX drops the answers of every device
        // whose jitter landed late, which is exactly the devices a real network has.
        let window_ms = if self.response_window_ms > 0 {
            self.response_window_ms
        } else {
            u64::from(pending.mx) * 1000
        };

        self.active = Some(ActiveSearch {
            st: pending.st.clone(),
            mx: pending.mx,
            target: pending.target,
            depth: pending.depth,
            deadline: Instant::now() + Duration::from_millis(window_ms),
            responders: Vec::new(),
            seen: HashSet::new(),
            duplicates: 0,
        });

        Ok(bytes)
    }

    /// Close the open window and tell the model what it found.
    async fn finish_search(&mut self) {
        let Some(search) = self.active.take() else {
            return;
        };
        let pending = PendingSearch {
            st: search.st,
            mx: search.mx,
            target: search.target,
            depth: search.depth,
        };
        self.complete_search(&pending, search.responders, search.duplicates, None)
            .await;
    }

    /// Raise `ssdp_search_complete` and act on the answer.
    async fn complete_search(
        &mut self,
        search: &PendingSearch,
        responders: Vec<serde_json::Value>,
        duplicates: usize,
        send_error: Option<String>,
    ) {
        let depth = search.depth + 1;
        info!(
            "SSDP client {} search for {} complete: {} responder(s), {} duplicate(s)",
            self.client_id,
            search.st,
            responders.len(),
            duplicates
        );
        let _ = self.status_tx.send(format!(
            "[CLIENT] SSDP client {} finished searching {}: {} device(s) answered",
            self.client_id,
            search.st,
            responders.len()
        ));
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());

        let mut data = serde_json::json!({
            "st": search.st,
            "mx": search.mx,
            "target": search.target.to_string(),
            "responder_count": responders.len(),
            "responders": responders,
            "duplicate_count": duplicates,
            "search_depth": depth,
        });
        if let Some(err) = send_error {
            data["send_error"] = serde_json::Value::String(err);
        }

        let event = Event::new(&SSDP_SEARCH_COMPLETE_EVENT, data);
        let actions = self.call_model(&event).await;
        if let Some(actions) = actions {
            self.apply_actions(actions, depth).await;
        }
    }

    // -----------------------------------------------------------------------
    // Inbound datagrams
    // -----------------------------------------------------------------------

    async fn handle_inbound(&mut self, inbound: Inbound) {
        let msg = match message::parse(&inbound.data) {
            Ok(msg) => msg,
            Err(e) => {
                // Dropped, not answered: a control point that reacted to malformed input
                // would be reacting to whatever is on the group, which on a real network is
                // plenty. Logged at DEBUG so it is visible when looked for.
                debug!(
                    "SSDP client {} dropping {} bytes from {}: {}",
                    self.client_id,
                    inbound.data.len(),
                    inbound.source,
                    e
                );
                return;
            }
        };

        match msg.method.as_deref() {
            // A status line is a device answering a search — ours, or somebody else's if we
            // are sitting on the group.
            None => self.handle_search_response(msg, inbound.source).await,
            Some("NOTIFY") => self.handle_notify(msg, inbound.source).await,
            Some("M-SEARCH") => {
                // Another control point searching. We are not a device; answering would be
                // a lie, and answering our own group traffic would be a loop.
                debug!(
                    "SSDP client {} ignoring an M-SEARCH from {} (we are a control point, \
                     not a device)",
                    self.client_id, inbound.source
                );
            }
            Some(other) => {
                warn!(
                    "SSDP client {} ignoring unexpected SSDP method '{}' from {}",
                    self.client_id, other, inbound.source
                );
            }
        }
    }

    async fn handle_search_response(&mut self, msg: HttpuMessage, source: SocketAddr) {
        // UDA 1.1 §1.3.3: the answer to an M-SEARCH is a 200. Anything else is a device
        // reporting a problem with a search, and treating it as a discovery would put a
        // device that refused us into the results.
        if !msg.start_line.contains(" 200") {
            warn!(
                "SSDP client {} ignoring non-200 response from {}: {:?}",
                self.client_id, source, msg.start_line
            );
            return;
        }

        let st = msg.header("ST").unwrap_or_default().to_string();
        let usn = msg.header("USN").unwrap_or_default().to_string();
        let source_address = source.to_string();

        // Dedupe within the open window only, keyed on address *and* USN: a device
        // retransmitting is one device, but a device answering `ssdp:all` with several
        // services is several genuinely distinct results.
        let depth = match self.active.as_mut() {
            Some(search) => {
                let key = (source_address.clone(), usn.clone());
                if !search.seen.insert(key) {
                    search.duplicates += 1;
                    debug!(
                        "SSDP client {} suppressing a repeat answer from {} (USN {})",
                        self.client_id, source_address, usn
                    );
                    return;
                }
                search.responders.push(serde_json::json!({
                    "st": st,
                    "usn": usn,
                    "location": msg.header("LOCATION"),
                    "server": msg.header("SERVER"),
                    "source_address": source_address,
                }));
                search.depth + 1
            }
            // No window open: a response overheard on the group, or one that arrived after
            // ours closed. Still worth telling the model about — it is a real device.
            None => 0,
        };

        info!(
            "SSDP client {} discovered {} at {} (ST {})",
            self.client_id,
            if usn.is_empty() {
                "a device"
            } else {
                usn.as_str()
            },
            msg.header("LOCATION").unwrap_or("<no LOCATION>"),
            if st.is_empty() { "<none>" } else { st.as_str() }
        );

        let event = Event::new(
            &SSDP_SEARCH_RESPONSE_EVENT,
            serde_json::json!({
                "st": st,
                "usn": usn,
                "location": msg.header("LOCATION"),
                "server": msg.header("SERVER"),
                "cache_control": msg.header("CACHE-CONTROL"),
                "cache_control_max_age": max_age_of(&msg),
                "source_address": source_address,
                "headers": msg.headers_json(),
            }),
        );
        let actions = self.call_model(&event).await;
        if let Some(actions) = actions {
            self.apply_actions(actions, depth).await;
        }
    }

    async fn handle_notify(&mut self, msg: HttpuMessage, source: SocketAddr) {
        let nt = msg.header("NT").unwrap_or_default().to_string();
        let nts = msg.header("NTS").unwrap_or_default().to_string();
        let usn = msg.header("USN").unwrap_or_default().to_string();

        info!(
            "SSDP client {} heard {} from {} (NT {}, USN {})",
            self.client_id,
            if nts.is_empty() {
                "an announcement"
            } else {
                nts.as_str()
            },
            source,
            if nt.is_empty() { "<none>" } else { nt.as_str() },
            usn
        );

        let event = Event::new(
            &SSDP_NOTIFY_RECEIVED_EVENT,
            serde_json::json!({
                "nt": nt,
                "nts": nts,
                "usn": usn,
                "location": msg.header("LOCATION"),
                "server": msg.header("SERVER"),
                "cache_control_max_age": max_age_of(&msg),
                "source_address": source.to_string(),
                "headers": msg.headers_json(),
            }),
        );
        // Announcements are unsolicited, so they start their own chain at depth 0.
        let actions = self.call_model(&event).await;
        if let Some(actions) = actions {
            self.apply_actions(actions, 0).await;
        }
    }

    // -----------------------------------------------------------------------
    // Model plumbing
    // -----------------------------------------------------------------------

    /// Ask the model what to do about `event`.
    ///
    /// `None` means there is nothing to act on — no instruction configured, or the call
    /// failed. It never means "the answer was discarded": every `Some` is executed by the
    /// caller, which is the one thing this repo's clients get wrong most often.
    async fn call_model(&mut self, event: &Event) -> Option<Vec<serde_json::Value>> {
        let instruction = self
            .app_state
            .get_instruction_for_client(self.client_id)
            .await?;

        // Bound rather than used directly as a `match` scrutinee: temporaries in a scrutinee
        // live for the whole match, so the `&self.protocol` / `&self.memory` borrows would
        // still be alive in the arm that assigns `self.memory`.
        let result = call_llm_for_client(
            &self.llm_client,
            &self.app_state,
            self.client_id.to_string(),
            &instruction,
            &self.memory,
            Some(event),
            &self.protocol,
            &self.status_tx,
        )
        .await;

        match result {
            Ok(ClientLlmResult {
                actions,
                memory_updates,
            }) => {
                if let Some(memory) = memory_updates {
                    self.memory = memory.clone();
                    self.app_state
                        .set_memory_for_client(self.client_id, memory)
                        .await;
                }
                Some(actions)
            }
            Err(e) => {
                error!(
                    "SSDP client {} LLM error on '{}': {}",
                    self.client_id,
                    event.id(),
                    e
                );
                let _ = self.status_tx.send(format!(
                    "[CLIENT] ✖ SSDP client {} could not get an answer for '{}': {}",
                    self.client_id,
                    event.id(),
                    e
                ));
                None
            }
        }
    }

    /// Execute everything the model returned, in order.
    async fn apply_actions(&mut self, actions: Vec<serde_json::Value>, depth: usize) {
        for action in actions {
            match self.apply_action(&action, depth).await {
                Ok(Applied::Disconnect) => {
                    info!("SSDP client {} disconnecting on request", self.client_id);
                    self.done = true;
                    return;
                }
                Ok(Applied::Refused(reason)) => {
                    error!(
                        "SSDP client {} refused an action: {}",
                        self.client_id, reason
                    );
                    let _ = self.status_tx.send(format!(
                        "[CLIENT] ⚠ SSDP client {}: {}",
                        self.client_id, reason
                    ));
                }
                Ok(Applied::Queued { .. }) | Ok(Applied::Waited) => {}
                Err(e) => {
                    error!(
                        "SSDP client {} could not execute action {}: {}",
                        self.client_id, action, e
                    );
                }
            }
        }
    }

    /// Execute one action. The single place an action becomes an effect, shared by the LLM
    /// path and the command channel.
    async fn apply_action(&mut self, action: &serde_json::Value, depth: usize) -> Result<Applied> {
        match self.protocol.execute_action(action.clone())? {
            ClientActionResult::Custom { name, data } if name == "send_msearch" => {
                if depth > MAX_FOLLOWUP_DEPTH {
                    return Ok(Applied::Refused(format!(
                        "refusing a {}-deep follow-up search (limit {}). Discovery is \
                         iterative and does not converge on its own; report what has been \
                         found instead of searching again.",
                        depth, MAX_FOLLOWUP_DEPTH
                    )));
                }
                let st = data["st"].as_str().unwrap_or_default().to_string();
                let mx = data["mx"].as_u64().unwrap_or(0) as u32;
                let target = match data["target"].as_str() {
                    // Already validated as a SocketAddr by the executor, so this cannot fail
                    // on model input; the fallback keeps a future refactor honest.
                    Some(s) => s.parse().unwrap_or(self.default_target),
                    None => self.default_target,
                };
                self.queue.push_back(PendingSearch {
                    st: st.clone(),
                    mx,
                    target,
                    depth,
                });
                Ok(Applied::Queued { st })
            }
            ClientActionResult::Custom { name, .. } => Ok(Applied::Refused(format!(
                "'{name}' is not an SSDP client verb"
            ))),
            ClientActionResult::WaitForMore => {
                trace!("SSDP client {} waiting for more", self.client_id);
                Ok(Applied::Waited)
            }
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            other => Ok(Applied::Refused(format!("no wire effect: {other:?}"))),
        }
    }

    // -----------------------------------------------------------------------
    // Injected commands (the dashboard's [ send ])
    // -----------------------------------------------------------------------

    /// Bespoke rather than `command_support::handle_stream_client_command`: that helper
    /// writes `SendData` to a write half, and every SSDP verb is a `Custom` over a shared
    /// `UdpSocket` with no write half at all. The logging and reply are what the helper does.
    async fn handle_command(&mut self, command: ClientCommand) {
        use crate::llm::actions::protocol_trait::Protocol;

        let action = command.action.clone();
        // Depth 0: an operator pressing [ send ] is starting a chain, not continuing one.
        //
        // Every arm is `Ok(..)`: a bad action is a `Rejected` outcome the caller can read,
        // not a transport failure. The annotation is what tells the `Err` arm of the
        // access-log conversion below what error type it is looking at.
        let outcome: anyhow::Result<ClientSendOutcome> = match self.apply_action(&action, 0).await {
            Ok(Applied::Queued { st }) => Ok(ClientSendOutcome::Executed {
                detail: format!("M-SEARCH for '{st}' queued"),
            }),
            Ok(Applied::Waited) => Ok(ClientSendOutcome::Executed {
                detail: "wait_for_more".to_string(),
            }),
            Ok(Applied::Disconnect) => {
                self.done = true;
                Ok(ClientSendOutcome::Disconnected)
            }
            Ok(Applied::Refused(reason)) => Ok(ClientSendOutcome::Rejected { error: reason }),
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
        };

        let outcome_json = match &outcome {
            Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };
        self.app_state
            .record_access_log(
                AccessLogOwner::Client(self.client_id.as_u32()),
                self.protocol.protocol_name(),
                None,
                "injected_action",
                action,
                vec![outcome_json],
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());

        crate::client::command_support::reply(command, outcome);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Render an `M-SEARCH * HTTP/1.1` request (UDA 1.1 §1.3.2).
///
/// `HOST` names the address the search is actually sent to. For the multicast form that is
/// the group, which is what the specification prints; for a unicast search it is the device,
/// which is what a device expects to see and what NetGet's own server test sends.
///
/// `MAN` keeps its quotes — `"ssdp:discover"` including them is what conforming control
/// points send, and devices have been observed to ignore searches without them.
///
/// Not in `server/ssdp/message.rs` because that file is the device's half and is not ours to
/// edit; the parsing side, which is the part with real grammar in it, *is* shared from there.
/// `pub` so `tests/client/ssdp/e2e_test.rs` can assert the exact bytes against the codec
/// without a socket: all tests live in `tests/`, so anything a test needs to see has to be
/// public here.
pub fn render_msearch(st: &str, mx: u32, target: SocketAddr, user_agent: &str) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {target}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx}\r\n\
         ST: {st}\r\n\
         USER-AGENT: {user_agent}\r\n\
         \r\n"
    )
}

/// The `max-age` directive of `CACHE-CONTROL`, as a number.
///
/// Given to the model already parsed: asked "is this device still fresh?", a model should not
/// first have to pull an integer out of a header directive.
pub fn max_age_of(msg: &HttpuMessage) -> Option<u64> {
    let value = msg.header("CACHE-CONTROL")?;
    value.split(',').find_map(|part| {
        let part = part.trim();
        let rest = part.strip_prefix("max-age")?.trim_start();
        let rest = rest.strip_prefix('=')?.trim();
        rest.parse::<u64>().ok()
    })
}

/// Turn a send error into something an operator can act on.
///
/// One failure dominates here and is deeply unobvious: sending to `239.255.255.250` from a
/// socket bound to loopback fails with `EADDRNOTAVAIL` because loopback carries no multicast
/// route — measured on macOS 27, where the *join* succeeds and only the send fails, which is
/// the opposite of the usual expectation. Left as the bare OS message it reads as a bug in
/// NetGet.
fn send_failure_hint(error: &anyhow::Error, target: SocketAddr, local: SocketAddr) -> String {
    let base = error.to_string();
    let target_is_multicast = match target.ip() {
        std::net::IpAddr::V4(ip) => ip.is_multicast(),
        std::net::IpAddr::V6(ip) => ip.is_multicast(),
    };
    if target_is_multicast && local.ip().is_loopback() {
        return format!(
            "{base} — the socket is bound to loopback ({local}) and loopback carries no \
             multicast route, so nothing can be sent to {target}. Either set startup param \
             bind_address to 0.0.0.0, or give send_msearch a unicast 'target'."
        );
    }
    base
}
