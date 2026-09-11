//! XMPP (Jabber) client implementation
pub mod actions;

pub use actions::XmppClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::xmpp::actions::{
    XMPP_CLIENT_CONNECTED_EVENT, XMPP_CLIENT_MESSAGE_RECEIVED_EVENT,
    XMPP_CLIENT_PRESENCE_RECEIVED_EVENT,
};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

use crate::console_error;
use futures::StreamExt;
use tokio_xmpp::connect::DnsConfig;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::{Client as XmppClient, Event as XmppEvent};
use xmpp_parsers::{
    message::{Lang, Message, MessageType},
    presence::{Presence, Show as PresenceShow, Type as PresenceType},
};

/// Connection state for LLM processing
#[derive(Debug, Clone, PartialEq)]
enum ConnectionState {
    Idle,
    Processing,
    Accumulating,
}

/// Per-client data for LLM handling
struct ClientData {
    state: ConnectionState,
    queued_events: Vec<XmppEvent>,
    memory: String,
}

/// XMPP client that connects to an XMPP/Jabber server
pub struct XmppClientConnection;

impl XmppClientConnection {
    /// Connect to an XMPP server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        let (jid, password) = Self::credentials(&remote_addr, startup_params.as_ref())?;
        let dns_config = Self::target(&remote_addr)?;

        info!(
            "XMPP client {} connecting as {} to {}",
            client_id, jid, dns_config
        );
        let _ = status_tx.send(format!(
            "[CLIENT] XMPP client {client_id} connecting to {dns_config}..."
        ));

        // `XmppClient::new` would ignore `dns_config` entirely and resolve
        // `_xmpp-client._tcp.<jid domain>` instead - see `target`.
        let mut xmpp_client = XmppClient::new_starttls(
            jid.clone(),
            password,
            dns_config,
            tokio_xmpp::xmlstream::Timeouts::default(),
        );

        // Store JID in app state
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field("jid".to_string(), serde_json::json!(jid.to_string()));
            })
            .await;

        // **`ClientStatus::Connected` is deliberately not set here.** It used to be, right at
        // this point - and constructing an `XmppClient` performs no I/O whatsoever: no TCP
        // connect, no STARTTLS, no SASL. So a client aimed at a dead host, a closed port, or a
        // server that rejected its password sat in the rail as `Connected` for ever, and the
        // rail had no way to distinguish it from a working one.
        //
        // `Event::Online` is the first point at which the stream is established, authenticated
        // and bound, and the event loop sets the status when it sees it. The instance stays
        // `Connecting` until then, which is what `client_startup` already created it as.
        //
        // Blocking here until Online would be the other way to make the status honest, and is
        // the wrong one: `tokio_xmpp` reconnects by itself, so an unreachable host is a state
        // to display rather than a reason to refuse - and blocking would hold up creation and
        // leave nothing draining the stanza channel, which is exactly what the injected-command
        // path needs. A target that cannot be *resolved* is different, and `target()` above
        // does return `Err` for it, before any of this.
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Initialize client data
        let client_data = Arc::new(Mutex::new(ClientData {
            state: ConnectionState::Idle,
            queued_events: Vec::new(),
            memory: String::new(),
        }));

        let protocol = Arc::new(crate::client::xmpp::actions::XmppClientProtocol::new());

        // Create channel for sending stanzas to the event loop
        let (stanza_tx, mut stanza_rx) = mpsc::unbounded_channel::<StanzaRequest>();
        let stanza_tx = Arc::new(stanza_tx);

        // Signals the event loop to shut the stream down cleanly (an injected `disconnect`).
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // The event loop signals the first `Online` here, carrying the JID the server actually
        // bound (which may differ in resource from the one we asked for). The connected-event
        // task waits on it, so `xmpp_connected` means "the stream is up" rather than "a value
        // was constructed" - and reports the bound JID rather than the requested one.
        let (online_tx, online_rx) = tokio::sync::oneshot::channel::<Jid>();
        let mut online_tx = Some(online_tx);

        // Command channel for injected actions (the dashboard's [ send ]). Registered before
        // the connected-event LLM call is even started, because a manual `*` rule parks that
        // call until a human answers it and [ send ] must work throughout.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_stanza_tx = stanza_tx.clone();
        let cmd_state = app_state.clone();
        let cmd_status_tx = status_tx.clone();
        let cmd_task = tokio::spawn(async move {
            Self::command_loop(
                command_rx,
                cmd_stanza_tx,
                shutdown_tx,
                client_id,
                cmd_state,
                cmd_status_tx,
            )
            .await;
        });
        app_state.register_client_task(client_id, cmd_task).await;

        // Call the LLM with the connected event in its **own** task rather than inline.
        // Inline, a parked (manual-rule) or slow call held up `connect()` itself, and - worse
        // for this feature - nothing would have been draining `stanza_rx` while it waited, so
        // an injected stanza could not have been written until the call finished.
        let connect_llm = llm_client.clone();
        let connect_state = app_state.clone();
        let connect_status_tx = status_tx.clone();
        let connect_protocol = protocol.clone();
        let connect_stanza_tx = stanza_tx.clone();
        let connect_data = client_data.clone();
        let connect_task = tokio::spawn(async move {
            // Wait for the stream to be up. Firing `xmpp_connected` before that told the model
            // it was connected when nothing had been dialled yet, so any `send_message` it
            // answered with was composed against a session that might never exist. If the
            // stream never comes up this task simply never proceeds; it is registered with
            // `register_client_task`, so `stop_client` aborts it.
            let Ok(bound_jid) = online_rx.await else {
                debug!(
                    "XMPP client {} never came online; no xmpp_connected event",
                    client_id
                );
                return;
            };
            let Some(instruction) = connect_state.get_instruction_for_client(client_id).await
            else {
                return;
            };
            let event = Event::new(
                &XMPP_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "jid": bound_jid.to_string(),
                }),
            );
            let memory = { connect_data.lock().await.memory.clone() };
            match call_llm_for_client(
                &connect_llm,
                &connect_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                connect_protocol.as_ref(),
                &connect_status_tx,
            )
            .await
            {
                Ok(ClientLlmResult {
                    actions,
                    memory_updates,
                }) => {
                    if let Some(mem) = memory_updates {
                        connect_data.lock().await.memory = mem;
                    }

                    // Execute initial actions
                    for action in actions {
                        Self::execute_action_result(
                            action,
                            connect_protocol.clone(),
                            connect_stanza_tx.clone(),
                            client_id,
                            &connect_status_tx,
                        )
                        .await;
                    }
                }
                Err(e) => {
                    error!("LLM error on XMPP connect for client {}: {}", client_id, e);
                }
            }
        });
        app_state
            .register_client_task(client_id, connect_task)
            .await;

        // Spawn event loop that handles both sending and receiving
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Shut the stream down cleanly on an injected `disconnect`.
                    _ = &mut shutdown_rx => {
                        info!("XMPP client {} closing the stream on request", client_id);
                        break;
                    }
                    // Handle outgoing stanzas
                    Some(request) = stanza_rx.recv() => {
                        // `send_stanza` resolves only once the stanza has been written to the
                        // XMPP transport, so an injected send can be reported truthfully
                        // rather than as "handed to a channel".
                        let result = xmpp_client
                            .send_stanza(request.stanza)
                            .await
                            .map(|_token| ())
                            .map_err(|e| e.to_string());
                        if let Err(e) = &result {
                            console_error!(status_tx, "Failed to send XMPP stanza: {}", e);
                        }
                        if let Some(ack) = request.ack {
                            let _ = ack.send(result);
                        }
                    }
                    // Handle incoming events
                    Some(xmpp_event) = xmpp_client.next() => {
                trace!("XMPP client {} received event: {:?}", client_id, xmpp_event);

                // Stream lifecycle is handled here rather than in `handle_xmpp_event`,
                // because it is not a stanza: it must reach the client's status immediately
                // instead of queueing behind an LLM call, and neither event has anything to
                // ask the model about.
                match &xmpp_event {
                    XmppEvent::Online { bound_jid, resumed } => {
                        // The one point at which this client is genuinely usable: stream
                        // established, SASL done, resource bound. `connect()` deliberately
                        // does not claim `Connected` - constructing the client does no I/O -
                        // so this is where the rail stops saying `Connecting`.
                        info!(
                            "XMPP client {} online as {} (resumed={})",
                            client_id, bound_jid, resumed
                        );
                        app_state
                            .with_client_mut(client_id, |client| {
                                client.set_protocol_field(
                                    "bound_jid".to_string(),
                                    serde_json::json!(bound_jid.to_string()),
                                );
                            })
                            .await;
                        app_state
                            .update_client_status(client_id, ClientStatus::Connected)
                            .await;
                        let _ = status_tx.send(format!(
                            "[CLIENT] XMPP client {client_id} online as {bound_jid}"
                        ));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        // Releases the connected-event task; a no-op on a reconnect, because
                        // the sender is taken the first time.
                        if let Some(tx) = online_tx.take() {
                            let _ = tx.send(bound_jid.clone());
                        }
                        continue;
                    }
                    XmppEvent::Disconnected(e) => {
                        // Not fatal - `tokio_xmpp` reconnects on its own - but the reason used
                        // to be bound as `_e` and thrown away under a bare `warn!`, so nothing
                        // recorded *why* a session dropped and the rail went on showing
                        // `Connected` through the outage.
                        warn!(
                            "XMPP client {} stream dropped: {} (reconnecting)",
                            client_id, e
                        );
                        app_state
                            .update_client_status(client_id, ClientStatus::Connecting)
                            .await;
                        let _ = status_tx.send(format!(
                            "[WARN] XMPP client {client_id} stream dropped, reconnecting: {e}"
                        ));
                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                        continue;
                    }
                    XmppEvent::Stanza(_) => {}
                }

                // Handle event with LLM
                let mut client_data_lock = client_data.lock().await;

                match client_data_lock.state {
                    ConnectionState::Idle => {
                        // Process immediately
                        client_data_lock.state = ConnectionState::Processing;
                        drop(client_data_lock);

                        Self::handle_xmpp_event(
                            xmpp_event,
                            &llm_client,
                            &app_state,
                            client_id,
                            &client_data,
                            protocol.clone(),
                            stanza_tx.clone(),
                            &status_tx,
                        ).await;

                        // Process queued events
                        let mut client_data_lock = client_data.lock().await;
                        let queued = std::mem::take(&mut client_data_lock.queued_events);
                        client_data_lock.state = ConnectionState::Idle;
                        drop(client_data_lock);

                        for queued_event in queued {
                            Self::handle_xmpp_event(
                                queued_event,
                                &llm_client,
                                &app_state,
                                client_id,
                                &client_data,
                                protocol.clone(),
                                stanza_tx.clone(),
                                &status_tx,
                            ).await;
                        }
                    }
                    ConnectionState::Processing => {
                        // Queue event
                        client_data_lock.queued_events.push(xmpp_event);
                        client_data_lock.state = ConnectionState::Accumulating;
                    }
                    ConnectionState::Accumulating => {
                        // Continue queuing
                        client_data_lock.queued_events.push(xmpp_event);
                    }
                }
                    }
                    // No more events - connection closed
                    else => {
                        info!("XMPP client {} connection closed", client_id);
                        break;
                    }
                }
            }

            info!("XMPP client {} disconnected", client_id);
            app_state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            // Every exit path lands here: drop the command handle so the dashboard stops
            // offering [ send ] on a dead connection, which also ends `command_loop`. Done
            // before the stream shutdown below, which waits on a peer that may be gone.
            app_state.remove_client_handle(client_id).await;
            // Orderly stream shutdown; harmless if the peer already went away, and bounded
            // because `close()` waits for a peer that never answers when the stream was never
            // established.
            match tokio::time::timeout(std::time::Duration::from_secs(5), xmpp_client.send_end())
                .await
            {
                Ok(Err(e)) => debug!("XMPP client {} stream close: {}", client_id, e),
                Err(_) => debug!("XMPP client {} stream close timed out", client_id),
                Ok(Ok(())) => {}
            }
            let _ = status_tx.send(format!("[CLIENT] XMPP client {} disconnected", client_id));
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        // Return dummy local address (XMPP handles this internally)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// The JID and password to authenticate with.
    ///
    /// The declared `jid` / `password` startup parameters are the documented way in, and until
    /// now they were **read by nothing**: `connect()` never passed `ctx.startup_params`, and
    /// this function looked in `protocol_data`, which is `Value::Null` at connect time and only
    /// ever written *after* it. So the only path that worked was the undocumented
    /// `user@domain@password` packed into `remote_addr` - which no startup example uses, so
    /// every documented way of starting this client failed at `connect()` with "Invalid XMPP
    /// address format".
    ///
    /// Both forms work now, in that order. The packed form is kept because it is the only way
    /// to supply credentials over an interface that has no startup parameters.
    fn credentials(
        remote_addr: &str,
        startup_params: Option<&crate::protocol::StartupParams>,
    ) -> Result<(Jid, String)> {
        let declared_jid = startup_params
            .map(|p| p.get_optional_string("jid"))
            .transpose()?
            .flatten();
        let declared_password = startup_params
            .map(|p| p.get_optional_string("password"))
            .transpose()?
            .flatten();

        let (jid_str, password) = match (declared_jid, declared_password) {
            (Some(j), Some(p)) => (j, p),
            (jid, password) => {
                // `user@domain@password`, with the password allowed to contain '@'.
                let parts: Vec<&str> = remote_addr.split('@').collect();
                if parts.len() < 3 {
                    let missing = match (jid.is_some(), password.is_some()) {
                        (true, false) => "the 'password' startup parameter is missing",
                        (false, true) => "the 'jid' startup parameter is missing",
                        _ => "neither the 'jid' nor the 'password' startup parameter was given",
                    };
                    anyhow::bail!(
                        "XMPP needs a JID and a password: {missing}, and remote_addr \
                         {remote_addr:?} is not the packed 'user@domain@password' form either. \
                         Pass startup_params {{\"jid\": \"alice@example.com\", \"password\": \"…\"}} \
                         alongside remote_addr \"host:port\"."
                    );
                }
                (
                    format!("{}@{}", parts[0], parts[1]),
                    parts[2..].join("@"),
                )
            }
        };

        let jid: Jid = jid_str
            .parse()
            .with_context(|| format!("Invalid JID {jid_str:?}"))?;
        Ok((jid, password))
    }

    /// Where to actually connect, derived from `remote_addr` and nothing else.
    ///
    /// **This is the defect that mattered most in this client.** `XmppClient::new` builds
    /// `DnsConfig::srv_default_client(jid.domain())` internally, so it resolved
    /// `_xmpp-client._tcp.<the JID's domain>` and connected *there* - `remote_addr` was parsed
    /// only to pull credentials out of and was otherwise discarded (it was even bound as
    /// `_server_addr`). An operator pointing this client at `127.0.0.1:5222` therefore got a
    /// connection to whatever that domain publishes on the public internet, and the supplied
    /// password was offered to it. That is the DynamoDB-client shape CLAUDE.md records: a
    /// client that loses its target and lets a library's default decide where the traffic goes.
    ///
    /// So the target comes from `remote_addr`, and if `remote_addr` is not usable this returns
    /// `Err` rather than falling back to SRV. `DnsConfig::Addr` when it is already an
    /// `IP:port`, `NoSrv` when it is a hostname (resolved as a plain A/AAAA lookup - still no
    /// SRV, because the operator named a host and meant it).
    fn target(remote_addr: &str) -> Result<DnsConfig> {
        // The packed credential form is `user@domain@password`; the connect target is then the
        // JID's domain on the default C2S port, because no other host was named.
        let addr = if remote_addr.split('@').count() >= 3 {
            let domain = remote_addr.split('@').nth(1).unwrap_or_default();
            if domain.is_empty() {
                anyhow::bail!(
                    "remote_addr {remote_addr:?} has no domain between the first two '@'"
                );
            }
            return Ok(DnsConfig::NoSrv {
                host: domain.to_string(),
                port: 5222,
            });
        } else {
            remote_addr.trim()
        };

        if addr.parse::<SocketAddr>().is_ok() {
            return Ok(DnsConfig::Addr {
                addr: addr.to_string(),
            });
        }

        let (host, port) = addr.rsplit_once(':').ok_or_else(|| {
            anyhow::anyhow!(
                "remote_addr {addr:?} must be \"host:port\" (e.g. \"127.0.0.1:5222\"). This \
                 client refuses to guess a target: falling back to an SRV lookup on the JID's \
                 domain would send the configured password to a host the operator did not name."
            )
        })?;
        let port: u16 = port
            .parse()
            .with_context(|| format!("remote_addr {addr:?} has an unusable port {port:?}"))?;
        if host.is_empty() {
            anyhow::bail!("remote_addr {addr:?} has an empty host");
        }
        Ok(DnsConfig::NoSrv {
            host: host.to_string(),
            port,
        })
    }

    /// Handle an XMPP event with LLM
    async fn handle_xmpp_event(
        xmpp_event: XmppEvent,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        client_id: ClientId,
        client_data: &Arc<Mutex<ClientData>>,
        protocol: Arc<XmppClientProtocol>,
        stanza_tx: Arc<mpsc::UnboundedSender<StanzaRequest>>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let event_opt = match xmpp_event {
            // Handled by the event loop before it gets here; see the `Online`/`Disconnected`
            // arms there. Kept as an explicit no-op rather than a catch-all so a new variant
            // in a future tokio-xmpp does not silently fall into it.
            XmppEvent::Online { .. } | XmppEvent::Disconnected(_) => None,
            XmppEvent::Stanza(stanza) => {
                // Match on stanza type to avoid clone
                use tokio_xmpp::Stanza;
                match stanza {
                    Stanza::Message(msg) => {
                        let from = msg.from.as_ref().map(|j| j.to_string()).unwrap_or_default();
                        let to = msg.to.as_ref().map(|j| j.to_string()).unwrap_or_default();
                        let body = msg
                            .bodies
                            .get(&Lang::default())
                            .cloned()
                            .unwrap_or_default();
                        let msg_type = format!("{:?}", msg.type_);

                        info!(
                            "XMPP client {} received message from {}: {}",
                            client_id, from, body
                        );

                        Some(Event::new(
                            &XMPP_CLIENT_MESSAGE_RECEIVED_EVENT,
                            serde_json::json!({
                                "from": from,
                                "to": to,
                                "body": body,
                                "message_type": msg_type,
                            }),
                        ))
                    }
                    Stanza::Presence(presence) => {
                        let from = presence
                            .from
                            .as_ref()
                            .map(|j| j.to_string())
                            .unwrap_or_default();
                        let presence_type = format!("{:?}", presence.type_);
                        let show = presence
                            .show
                            .as_ref()
                            .map(|s| format!("{:?}", s))
                            .unwrap_or_default();
                        let status = presence
                            .statuses
                            .get(&Lang::default())
                            .cloned()
                            .unwrap_or_default();

                        debug!(
                            "XMPP client {} received presence from {}: {:?}",
                            client_id, from, presence_type
                        );

                        Some(Event::new(
                            &XMPP_CLIENT_PRESENCE_RECEIVED_EVENT,
                            serde_json::json!({
                                "from": from,
                                "presence_type": presence_type,
                                "show": show,
                                "status": status,
                            }),
                        ))
                    }
                    Stanza::Iq(_iq) => {
                        debug!(
                            "XMPP client {} received IQ stanza (not yet supported)",
                            client_id
                        );
                        None
                    }
                }
            }
        };

        if let Some(event) = event_opt {
            if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
                match call_llm_for_client(
                    llm_client,
                    app_state,
                    client_id.to_string(),
                    &instruction,
                    &client_data.lock().await.memory,
                    Some(&event),
                    protocol.as_ref(),
                    status_tx,
                )
                .await
                {
                    Ok(ClientLlmResult {
                        actions,
                        memory_updates,
                    }) => {
                        if let Some(mem) = memory_updates {
                            client_data.lock().await.memory = mem;
                        }

                        // Execute actions
                        for action in actions {
                            Self::execute_action_result(
                                action,
                                protocol.clone(),
                                stanza_tx.clone(),
                                client_id,
                                status_tx,
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        error!("LLM error for XMPP client {}: {}", client_id, e);
                    }
                }
            }
        }
    }

    /// Execute an action and put whatever stanza it produced on the wire.
    ///
    /// Thin wrapper over [`Self::apply_action`] for the LLM paths, which have nowhere to
    /// report an outcome and only log.
    async fn execute_action_result(
        action: serde_json::Value,
        protocol: Arc<XmppClientProtocol>,
        stanza_tx: Arc<mpsc::UnboundedSender<StanzaRequest>>,
        client_id: ClientId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::Client;

        match protocol.as_ref().execute_action(action) {
            Ok(result) => {
                // The LLM path does not wait for the write: it has no caller to answer, and
                // waiting would serialise every action behind the event loop's next turn.
                match Self::apply_action(result, &stanza_tx, client_id, false).await {
                    Ok(applied) => trace!("XMPP client {}: {}", client_id, applied.detail()),
                    Err(e) => {
                        console_error!(status_tx, "XMPP client {} action failed: {}", client_id, e)
                    }
                }
            }
            Err(e) => {
                error!(
                    "Action execution error for XMPP client {}: {}",
                    client_id, e
                );
            }
        }
    }

    /// Turn one executed action into a stanza and hand it to the event loop.
    ///
    /// Shared by the connected-event path, the incoming-stanza path and injected commands, so
    /// the mapping from `send_message`/`send_presence` onto `xmpp_parsers` stanzas exists
    /// exactly once. With `wait_for_write` set the caller is told whether the stanza actually
    /// reached the transport, which is what an injected command needs to answer honestly.
    async fn apply_action(
        action_result: crate::llm::actions::client_trait::ClientActionResult,
        stanza_tx: &Arc<mpsc::UnboundedSender<StanzaRequest>>,
        client_id: ClientId,
        wait_for_write: bool,
    ) -> Result<XmppApplied> {
        use crate::llm::actions::client_trait::ClientActionResult;
        use tokio_xmpp::Stanza;

        match action_result {
            ClientActionResult::Custom { name, data } => match name.as_str() {
                "send_message" => {
                    let to = data
                        .get("to")
                        .and_then(|v| v.as_str())
                        .context("send_message result has no 'to'")?;
                    let body = data
                        .get("body")
                        .and_then(|v| v.as_str())
                        .context("send_message result has no 'body'")?;
                    let jid: Jid = to.parse().context("Invalid JID")?;

                    let mut message = Message::new(Some(jid));
                    message.type_ = MessageType::Chat;
                    message.bodies.insert(Lang::default(), body.to_string());

                    Self::dispatch(
                        Stanza::Message(message),
                        stanza_tx,
                        wait_for_write,
                        format!("<message/> to {} ({} byte body)", to, body.len()),
                    )
                    .await
                }
                "send_presence" => {
                    let show = data.get("show").and_then(|v| v.as_str());
                    let status = data.get("status").and_then(|v| v.as_str());

                    let mut presence = Presence::new(PresenceType::None);
                    if let Some(show_str) = show {
                        presence.show = match show_str {
                            "away" => Some(PresenceShow::Away),
                            "chat" => Some(PresenceShow::Chat),
                            "dnd" => Some(PresenceShow::Dnd),
                            "xa" => Some(PresenceShow::Xa),
                            _ => None,
                        };
                    }
                    if let Some(status_str) = status {
                        presence
                            .statuses
                            .insert(Lang::default(), status_str.to_string());
                    }

                    Self::dispatch(
                        Stanza::Presence(presence),
                        stanza_tx,
                        wait_for_write,
                        format!("<presence/> show={:?} status={:?}", show, status),
                    )
                    .await
                }
                other => Err(anyhow::anyhow!(
                    "XMPP client cannot apply custom result '{}'",
                    other
                )),
            },
            ClientActionResult::Disconnect => {
                info!("XMPP client {} disconnecting", client_id);
                Ok(XmppApplied::Disconnect)
            }
            ClientActionResult::WaitForMore => Ok(XmppApplied::NoWire(
                "waiting for the next stanza; nothing sent".to_string(),
            )),
            ClientActionResult::NoAction => {
                Ok(XmppApplied::NoWire("no_action: nothing sent".to_string()))
            }
            ClientActionResult::SendData(_) => Err(anyhow::anyhow!(
                "XMPP speaks stanzas, not raw bytes; use send_message"
            )),
            ClientActionResult::Multiple(_) => Err(anyhow::anyhow!(
                "XMPP client does not support Multiple action results"
            )),
        }
    }

    /// Hand one stanza to the event loop, optionally waiting until it has been written.
    async fn dispatch(
        stanza: tokio_xmpp::Stanza,
        stanza_tx: &Arc<mpsc::UnboundedSender<StanzaRequest>>,
        wait_for_write: bool,
        what: String,
    ) -> Result<XmppApplied> {
        if !wait_for_write {
            stanza_tx
                .send(StanzaRequest { stanza, ack: None })
                .map_err(|_| anyhow::anyhow!("XMPP connection is already closed"))?;
            return Ok(XmppApplied::Queued(format!("{what} queued for the stream")));
        }

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        stanza_tx
            .send(StanzaRequest {
                stanza,
                ack: Some(ack_tx),
            })
            .map_err(|_| anyhow::anyhow!("XMPP connection is already closed"))?;
        match ack_rx.await {
            Ok(Ok(())) => Ok(XmppApplied::Wrote(format!(
                "{what} written to the XMPP transport"
            ))),
            Ok(Err(e)) => Err(anyhow::anyhow!("send_stanza failed: {e}")),
            Err(_) => Err(anyhow::anyhow!(
                "the XMPP event loop ended before the stanza could be written"
            )),
        }
    }

    /// Drain injected commands (the dashboard's \[ send \]) until the channel closes - the
    /// client was removed, or the event loop exited - or an injected `disconnect` ends the
    /// session.
    ///
    /// `tokio_xmpp::Client` is owned by the event loop task and is not clonable, so the
    /// command loop reaches the stream the way every other producer here does: through the
    /// stanza channel. What makes the outcome honest is the per-stanza ack - the event loop
    /// reports back what `send_stanza` returned, and `send_stanza` resolves only once the
    /// stanza has reached the transport.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<crate::state::client_handles::ClientCommand>,
        stanza_tx: Arc<mpsc::UnboundedSender<StanzaRequest>>,
        shutdown_tx: tokio::sync::oneshot::Sender<()>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::Client;
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::client_handles::ClientSendOutcome;
        use crate::state::AccessLogOwner;

        let protocol = XmppClientProtocol::new();
        let mut shutdown_tx = Some(shutdown_tx);

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome: Result<ClientSendOutcome> = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => match Self::apply_action(result, &stanza_tx, client_id, true).await {
                    // Never `Sent`: the stanza really did reach the transport, but tokio-xmpp
                    // serialises and writes it internally and reports no byte count, so there
                    // is no honest number for `bytes_sent`.
                    Ok(XmppApplied::Wrote(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                    Ok(XmppApplied::Queued(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                    Ok(XmppApplied::NoWire(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                    Ok(XmppApplied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                    Err(e) => Err(e),
                },
            };

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            if let Err(e) = &outcome {
                error!("XMPP client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                // Tell the event loop to close the stream; it then runs its normal
                // disconnect path (status, handle removal, status_tx). The handle is dropped
                // here as well so [ send ] disappears immediately rather than after the
                // stream shutdown.
                if let Some(tx) = shutdown_tx.take() {
                    let _ = tx.send(());
                }
                app_state.remove_client_handle(client_id).await;
                break;
            }
        }
    }
}

/// One stanza on its way to the event loop, with an optional slot for the write result.
///
/// The ack is what lets an injected command report the truth: `Client::send_stanza` resolves
/// only once the stanza has been written to the transport, and this carries that result back
/// to whoever asked for the send.
struct StanzaRequest {
    stanza: tokio_xmpp::Stanza,
    ack: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
}

/// What [`XmppClientConnection::apply_action`] did with one executed action.
enum XmppApplied {
    /// Written to the XMPP transport (the caller asked to wait for it).
    Wrote(String),
    /// Handed to the event loop without waiting.
    Queued(String),
    /// Ran, but nothing goes on the wire.
    NoWire(String),
    /// The session should end.
    Disconnect,
}

impl XmppApplied {
    fn detail(&self) -> &str {
        match self {
            XmppApplied::Wrote(d) | XmppApplied::Queued(d) | XmppApplied::NoWire(d) => d,
            XmppApplied::Disconnect => "disconnect requested",
        }
    }
}
