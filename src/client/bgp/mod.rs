//! BGP client implementation (query mode)
//!
//! This BGP client connects to BGP peers to query routing information. It completes the
//! RFC 4271 session establishment (OPEN / KEEPALIVE) and then reports what the peer sends.
//! It maintains no RIB and announces nothing on its own.
//!
//! # Wire format
//!
//! Every byte on this path is produced and consumed by [`crate::server::bgp::wire`], the same
//! `netgauze-bgp-pkt`-backed codec the BGP *server* uses. Both halves live behind the same
//! `bgp` Cargo feature, so there is nothing to gate and nothing to duplicate.
//!
//! This client used to hand-roll its own encoder and decoder and carried three defects the
//! server half has since had fixed:
//!
//! * `&header_buf[0..16] != &BGP_MARKER` sliced a buffer before checking its length.
//!   [`wire::parse_header`] takes a `[u8; 19]`, so the bound is in the type, and it rejects a
//!   length outside `[19, 4096]` *before* a body buffer is allocated.
//! * `local_as as u16` silently turned AS 4200000000 into AS 60416 — a different, entirely
//!   valid-looking ASN, with no four-octet-AS capability and no diagnostic.
//!   [`wire::build_open`] puts `AS_TRANS` (23456) in the two-octet field and the real ASN in
//!   capability 65, per RFC 6793.
//! * UPDATE bodies reached the model as `hex::encode(body)`, which no model can act on. They
//!   are now decoded field by field by [`wire::update_to_json`].

pub mod actions;
pub use actions::BgpClientProtocol;

use anyhow::{anyhow, bail, Context, Result};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::{debug, error, info, trace, warn};

use netgauze_bgp_pkt::{capabilities::BgpCapability, BgpMessage};

use crate::client::bgp::actions::{
    BGP_CLIENT_CONNECTED_EVENT, BGP_CLIENT_NOTIFICATION_RECEIVED_EVENT,
    BGP_CLIENT_UPDATE_RECEIVED_EVENT,
};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::{Event, StartupParams};
use crate::server::bgp::wire;
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

const DEFAULT_HOLD_TIME: u16 = 180;
/// Private ASN, safe to use for monitoring.
const DEFAULT_LOCAL_AS: u32 = 65000;
const DEFAULT_ROUTER_ID: &str = "192.168.1.100";
/// RFC 4271 section 4.2: a non-zero hold time below three seconds is unacceptable.
const MIN_HOLD_TIME: u16 = 3;

/// RFC 4271 section 8.2.2: having sent our OPEN we are in OpenSent, where the Hold Timer is set
/// to "a large value" — the RFC suggests 4 minutes — because the *negotiated* timer cannot start
/// until the peer's OPEN has arrived.
///
/// Without it, a peer that accepted the TCP connection and then said nothing left this client
/// parked in `read_exact` indefinitely: `spawn_timers` is only reached from the OPEN handler, so
/// there was no timer of any kind covering the one phase the peer controls entirely.
const OPEN_HOLD_TIME: std::time::Duration = std::time::Duration::from_secs(240);

/// Where a write half lives once the read loop has been spawned.
type SharedWriter = Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>;

/// BGP session state, from the client's point of view.
///
/// The full RFC 4271 FSM has six states; this client never listens and never retries, so
/// `Idle` and `Active` cannot occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgpState {
    OpenSent,
    OpenConfirm,
    Established,
}

/// Per-client data for LLM handling.
struct ClientData {
    bgp_state: BgpState,
    memory: String,
    peer_as: Option<u32>,
    peer_router_id: Option<String>,
    /// Whether the peer advertised the four-octet-AS capability. Decides how AS_PATH in an
    /// inbound UPDATE is read; getting it wrong yields a wrong AS path rather than an error.
    peer_asn4: bool,
    /// The hold time we proposed in our OPEN.
    proposed_hold_time: u16,
    /// `min(ours, theirs)`, valid once the peer's OPEN has been seen.
    negotiated_hold_time: u16,
}

/// RFC 4271 section 4.2: the negotiated hold time is the smaller of the two proposals, and
/// zero on either side turns the timers off for both.
fn negotiate_hold(ours: u16, theirs: u16) -> u16 {
    ours.min(theirs)
}

/// Session shutdown signal, mirroring the BGP server's (`src/server/bgp/mod.rs`).
///
/// The flag is the source of truth and the `Notify` only interrupts a parked read. `Notify`
/// alone is not enough: `notify_waiters` wakes tasks that are *already* waiting, so a signal
/// raised while the read loop is inside an LLM call would be dropped and the session would hang
/// until the peer closed the socket.
///
/// This is what makes hold-timer enforcement possible at all on the client. The read loop parks
/// in `read_exact`, which no ticker can interrupt on its own; the loop selects over this
/// `Notify` and the read, so an expiry preempts a blocked read instead of waiting for TCP to
/// notice a peer that may never come back.
#[derive(Default)]
struct Shutdown {
    flag: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// [`with_deadline`] gave up before the future finished.
struct DeadlineExpired;

/// Await `fut`, abandoning it after `limit` when one is given.
///
/// A plain `tokio::time::timeout` cannot express "no limit", and only the pre-OPEN reads are
/// bounded from the read loop: once the session is negotiated, `spawn_timers` owns the schedule
/// and signals through `Shutdown` instead.
async fn with_deadline<F: std::future::Future>(
    limit: Option<std::time::Duration>,
    fut: F,
) -> std::result::Result<F::Output, DeadlineExpired> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, fut)
            .await
            .map_err(|_| DeadlineExpired),
        None => Ok(fut.await),
    }
}

/// BGP client that connects to a BGP peer
pub struct BgpClient;

impl BgpClient {
    /// Connect to a BGP peer with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        // Startup parameters are validated here rather than at first use, so a bad value fails
        // the connect with a message naming the parameter instead of quietly becoming a
        // different, valid-looking value on the wire.
        let local_as = startup_params
            .as_ref()
            .map(|p| p.get_optional_u32("local_as"))
            .transpose()?
            .flatten()
            .unwrap_or(DEFAULT_LOCAL_AS);
        if local_as == 0 {
            bail!("BGP client local_as must be 1-4294967295, got 0");
        }

        let router_id_str = startup_params
            .as_ref()
            .map(|p| p.get_optional_string("router_id"))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| DEFAULT_ROUTER_ID.to_string());
        let router_id: Ipv4Addr = router_id_str.parse().with_context(|| {
            format!("BGP client router_id {router_id_str:?} is not an IPv4 address")
        })?;
        if router_id.is_unspecified() {
            bail!("BGP client router_id must not be 0.0.0.0 (RFC 4271 requires a valid BGP Identifier)");
        }

        let hold_time_raw = startup_params
            .as_ref()
            .map(|p| p.get_optional_u32("hold_time"))
            .transpose()?
            .flatten()
            .unwrap_or(u32::from(DEFAULT_HOLD_TIME));
        if hold_time_raw > u32::from(u16::MAX) {
            bail!(
                "BGP client hold_time must be 0-65535 seconds, got {}",
                hold_time_raw
            );
        }
        let hold_time = hold_time_raw as u16;
        if hold_time != 0 && hold_time < MIN_HOLD_TIME {
            bail!(
                "BGP client hold_time must be 0 (timers disabled) or at least {} seconds, got {}",
                MIN_HOLD_TIME,
                hold_time
            );
        }

        info!(
            "BGP client {} connecting with AS={}, router_id={}, hold_time={}s",
            client_id, local_as, router_id, hold_time
        );

        // Connect to BGP peer (typically port 179)
        let stream = TcpStream::connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to BGP peer {remote_addr}"))?;

        let local_addr = stream.local_addr()?;
        let remote_sock_addr = stream.peer_addr()?;

        info!(
            "BGP client {} connected to {} (local: {})",
            client_id, remote_sock_addr, local_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] BGP client {client_id} connected"));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Split stream (never clone a TcpStream)
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half_arc: SharedWriter = Arc::new(Mutex::new(write_half));

        let client_data = Arc::new(Mutex::new(ClientData {
            bgp_state: BgpState::OpenSent,
            memory: String::new(),
            peer_as: None,
            peer_router_id: None,
            peer_asn4: false,
            proposed_hold_time: hold_time,
            negotiated_hold_time: hold_time,
        }));

        // Send our OPEN immediately. `wire::build_open` advertises the four-octet-AS
        // capability unconditionally and substitutes AS_TRANS in the two-octet field when the
        // real ASN does not fit, so a 32-bit ASN survives the trip.
        let open_msg = wire::encode(wire::build_open(local_as, hold_time, router_id))
            .context("Failed to encode BGP OPEN")?;
        write_half_arc
            .lock()
            .await
            .write_all(&open_msg)
            .await
            .context("Failed to send BGP OPEN")?;

        info!(
            "BGP client {} sent OPEN: AS={}, hold_time={}s, router_id={}",
            client_id, local_as, hold_time, router_id
        );
        let _ = status_tx.send(format!(
            "[CLIENT] BGP OPEN sent: AS={local_as}, hold_time={hold_time}s"
        ));

        let shutdown = Arc::new(Shutdown::default());

        // Command channel for injected actions (the dashboard's [ send_keepalive ] /
        // [ send_notification ] / [ disconnect ] rows). Registered BEFORE the read loop — and
        // so before the `bgp_connected` LLM call, which a manual `*` rule can park for minutes.
        //
        // `read_exact` is not cancellation-safe, so the commands are drained by their own task
        // rather than a `select!` arm in the read loop; both share the write half. Every wire
        // verb of this client yields `SendData` / `Disconnect`, so the generic arm covers the
        // whole vocabulary. On an injected `disconnect` the Cease NOTIFICATION is already on
        // the wire; the task raises `shutdown` so the read loop runs its normal teardown (which
        // shuts the write half down) instead of waiting for the peer to notice.
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_state = app_state.clone();
        let cmd_tx = status_tx.clone();
        let cmd_write = write_half_arc.clone();
        let cmd_shutdown = shutdown.clone();
        let cmd_task = tokio::spawn(async move {
            while let Some(cmd) = command_rx.recv().await {
                let disconnect = crate::client::command_support::handle_stream_client_command(
                    &BgpClientProtocol::new(),
                    &cmd_write,
                    cmd,
                    client_id,
                    &cmd_state,
                    &cmd_tx,
                )
                .await;
                if disconnect {
                    cmd_shutdown.set();
                    let _ = cmd_write.lock().await.shutdown().await;
                    break;
                }
            }
        });
        // Aborting the read-loop task does not abort tasks it spawned, and this one is a
        // sibling anyway: register it so stop/remove aborts it too.
        app_state.register_client_task(client_id, cmd_task).await;

        // Spawn read loop
        let client_data_clone = client_data.clone();
        let write_half_clone = write_half_arc.clone();
        let llm_client_clone = llm_client.clone();
        let app_state_clone = app_state.clone();
        let status_tx_clone = status_tx.clone();

        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn(async move {
            if let Err(e) = Self::read_loop(
                read_half,
                write_half_clone,
                client_id,
                llm_client_clone,
                app_state_clone,
                status_tx_clone,
                client_data_clone,
                shutdown,
            )
            .await
            {
                error!("BGP client {} read loop error: {}", client_id, e);
            }
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }

    /// Read loop for BGP messages
    #[allow(clippy::too_many_arguments)]
    async fn read_loop(
        mut read_half: tokio::io::ReadHalf<TcpStream>,
        write_half: SharedWriter,
        client_id: ClientId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_data: Arc<Mutex<ClientData>>,
        shutdown: Arc<Shutdown>,
    ) -> Result<()> {
        let last_received = Arc::new(AtomicU64::new(0));
        let started = std::time::Instant::now();

        let mut timer_task: Option<tokio::task::AbortHandle> = None;
        let result = Self::read_loop_inner(
            &mut read_half,
            &write_half,
            client_id,
            &llm_client,
            &app_state,
            &status_tx,
            &client_data,
            &mut timer_task,
            &shutdown,
            &last_received,
            started,
        )
        .await;

        // Teardown, in this order: stop the timers first so nothing can write to a socket that
        // is about to close, then shut the write half down explicitly. Relying on the `Arc`
        // refcount to drop the write half would leave the peer waiting for a FIN that only
        // arrives once every clone happens to go away.
        if let Some(task) = timer_task {
            task.abort();
        }
        let _ = write_half.lock().await.shutdown().await;

        // Every exit path of the read loop lands here: drop the command handle so the dashboard
        // stops offering [ send ] on a dead session (a late send then fails fast).
        app_state.remove_client_handle(client_id).await;
        app_state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_loop_inner(
        read_half: &mut tokio::io::ReadHalf<TcpStream>,
        write_half: &SharedWriter,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        timer_task: &mut Option<tokio::task::AbortHandle>,
        shutdown: &Arc<Shutdown>,
        last_received: &Arc<AtomicU64>,
        started: std::time::Instant,
    ) -> Result<()> {
        loop {
            if shutdown.is_set() {
                break;
            }

            // The negotiated four-octet-AS state decides how AS_PATH is read, and the session
            // state decides whether the pre-OPEN deadline applies. Copy both out rather than
            // holding the guard across the read.
            let (peer_asn4, open_deadline) = {
                let data = client_data.lock().await;
                (
                    data.peer_asn4,
                    (data.bgp_state == BgpState::OpenSent).then_some(OPEN_HOLD_TIME),
                )
            };

            let mut header = [0u8; wire::BGP_HEADER_LEN];
            // A parked `read_exact` is exactly what a hold timer has to be able to interrupt:
            // the peer that has to be dropped is by definition the one sending nothing. The
            // timer task raises `shutdown` after writing its NOTIFICATION, and this select
            // turns that into an immediate exit rather than a wait for TCP to give up. Before
            // the peer's OPEN there is no timer task yet, so the deadline above stands in.
            let read_result = tokio::select! {
                biased;
                _ = shutdown.notify.notified() => break,
                r = with_deadline(open_deadline, read_half.read_exact(&mut header)) => match r {
                    Ok(r) => r,
                    Err(DeadlineExpired) => {
                        error!(
                            "BGP client {} received no OPEN within {}s, closing (RFC 4271 8.2.2)",
                            client_id,
                            OPEN_HOLD_TIME.as_secs()
                        );
                        let _ = status_tx.send(format!(
                            "[CLIENT] BGP peer sent no OPEN within {}s, closing session",
                            OPEN_HOLD_TIME.as_secs()
                        ));
                        Self::send_notification(write_half, wire::ERR_HOLD_TIMER_EXPIRED, 0, &[])
                            .await;
                        break;
                    }
                },
            };

            match read_result {
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    info!("BGP client {} disconnected", client_id);
                    app_state
                        .update_client_status(client_id, ClientStatus::Disconnected)
                        .await;
                    let _ = status_tx.send(format!("[CLIENT] BGP client {client_id} disconnected"));
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    break;
                }
                Err(e) => {
                    error!("BGP client {} read error: {}", client_id, e);
                    return Err(e.into());
                }
            }

            // Marker, total length and per-type minimum are all validated before a single
            // body byte is read, so `len - BGP_HEADER_LEN` cannot underflow and the peer
            // cannot choose the buffer size.
            let (msg_len, msg_type) = match wire::parse_header(&header) {
                Ok(v) => v,
                Err(e) => {
                    let (code, subcode) = e.notify_code();
                    error!(
                        "BGP client {} rejected message header: {:?} ({}/{})",
                        client_id, e, code, subcode
                    );
                    let _ = status_tx.send(format!(
                        "[CLIENT] BGP header rejected: {} / {}",
                        wire::error_name(code),
                        wire::error_subcode_name(code, subcode)
                    ));
                    Self::send_notification(write_half, code, subcode, &e.notify_data()).await;
                    return Err(anyhow!("Invalid BGP message header: {e:?}"));
                }
            };

            let mut full = vec![0u8; msg_len];
            full[..wire::BGP_HEADER_LEN].copy_from_slice(&header);
            if msg_len > wire::BGP_HEADER_LEN {
                // The body read has to be under the same select as the header read. It was not,
                // and that left a hole the hold timer could not close: a peer that sent a valid
                // 19-octet header announcing a 4096-octet message and then stopped parked this
                // `read_exact` forever. `shutdown` was raised on expiry and nothing observed it,
                // so the NOTIFICATION went out and the session stayed up regardless. Half a
                // message is not worth preserving, so abandoning the read mid-way is fine —
                // every path out of here ends the session.
                let body = tokio::select! {
                    biased;
                    _ = shutdown.notify.notified() => break,
                    r = with_deadline(
                        open_deadline,
                        read_half.read_exact(&mut full[wire::BGP_HEADER_LEN..]),
                    ) => r,
                };
                match body {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => return Err(e).context("BGP message body truncated"),
                    Err(DeadlineExpired) => {
                        error!(
                            "BGP client {} stalled mid-message before the OPEN exchange, closing",
                            client_id
                        );
                        Self::send_notification(write_half, wire::ERR_HOLD_TIMER_EXPIRED, 0, &[])
                            .await;
                        break;
                    }
                }
            }

            // RFC 4271 section 6.5 / section 10: the Hold Timer restarts on every KEEPALIVE,
            // UPDATE or NOTIFICATION received. Recording it here, once the whole message is off
            // the wire, covers all three without a per-type branch — and a peer that keeps
            // talking is a peer that must not be dropped.
            last_received.store(started.elapsed().as_secs(), Ordering::Relaxed);

            trace!(
                "BGP client {} received message type={} length={}",
                client_id,
                msg_type,
                msg_len
            );

            // NOTIFICATION is handled from the raw octets rather than through netgauze, for
            // the same reason the server encodes it by hand: netgauze models it as a closed
            // enum of the (code, subcode) pairs it knows and cannot represent an arbitrary
            // subcode, and a peer is free to send one. `parse_header` has already guaranteed
            // at least 21 octets for this type, so both indexes are in range.
            if msg_type == wire::MSG_NOTIFICATION {
                let error_code = full[wire::BGP_HEADER_LEN];
                let error_subcode = full[wire::BGP_HEADER_LEN + 1];
                Self::handle_notification_message(
                    error_code,
                    error_subcode,
                    client_id,
                    llm_client,
                    app_state,
                    status_tx,
                    client_data,
                    shutdown,
                )
                .await;
                // RFC 4271 section 4.5: a NOTIFICATION closes the connection and is never
                // answered.
                break;
            }

            let message = match wire::decode(&full, peer_asn4) {
                Ok(m) => m,
                Err(e) => {
                    error!("BGP client {} could not parse message: {}", client_id, e);
                    let _ = status_tx.send(format!("[CLIENT] BGP parse error: {e}"));
                    let (code, subcode) = e.notify;
                    Self::send_notification(write_half, code, subcode, &[]).await;
                    return Err(anyhow!("Malformed BGP message: {e}"));
                }
            };

            match message {
                BgpMessage::Open(open) => {
                    Self::handle_open_message(open, write_half, client_id, status_tx, client_data)
                        .await?;

                    // RFC 4271 section 8.2.2: the timers start at OpenConfirm, not at
                    // Established. From the moment our KEEPALIVE went out the peer is entitled
                    // to expect the negotiated cadence from us — and we are entitled to expect
                    // one back, which is what the hold timer enforces. A second OPEN leaves
                    // `timer_task` already set and is ignored here as it is there.
                    if timer_task.is_none() {
                        let hold = client_data.lock().await.negotiated_hold_time;
                        if let Some(handle) = Self::spawn_timers(
                            write_half.clone(),
                            hold,
                            client_id,
                            last_received.clone(),
                            started,
                            shutdown.clone(),
                            status_tx.clone(),
                        ) {
                            // The abort handle stays here for normal teardown; the `JoinHandle`
                            // itself goes to `AppState` so `remove_client` aborts it too.
                            // Aborting the read-loop task does NOT abort tasks it spawned, so an
                            // unregistered timer task would outlive the client, holding the
                            // write half — and with it the socket — open while it kept sending
                            // keepalives to a peer nobody is reading.
                            *timer_task = Some(handle.abort_handle());
                            app_state.register_client_task(client_id, handle).await;
                        }
                    }
                }
                BgpMessage::KeepAlive => {
                    Self::handle_keepalive_message(
                        client_id,
                        llm_client,
                        app_state,
                        status_tx,
                        client_data,
                        write_half,
                        shutdown,
                    )
                    .await?;
                }
                BgpMessage::Update(update) => {
                    Self::handle_update_message(
                        &update,
                        client_id,
                        llm_client,
                        app_state,
                        status_tx,
                        client_data,
                        write_half,
                        shutdown,
                    )
                    .await;
                }
                BgpMessage::RouteRefresh(_) => {
                    // This client does not advertise the route-refresh capability, so a
                    // conforming peer will not send one. Ignoring it beats tearing down a
                    // healthy session over a message with no RIB behind it.
                    warn!(
                        "BGP client {} ignoring ROUTE-REFRESH: no RIB to re-advertise",
                        client_id
                    );
                }
                // Handled above from the raw octets; `decode` is never reached for it.
                BgpMessage::Notification(_) => break,
            }
        }

        Ok(())
    }

    /// Handle the peer's OPEN: validate, negotiate, and complete the handshake.
    async fn handle_open_message(
        open: netgauze_bgp_pkt::open::BgpOpenMessage,
        write_half: &SharedWriter,
        client_id: ClientId,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
    ) -> Result<()> {
        // Version, hold-time bounds and BGP Identifier validity are checked by the decoder;
        // reaching here means they passed. What is left is what only we can judge.
        //
        // `my_asn4()` is the peer's *real* ASN: the four-octet-AS capability when it sent one,
        // otherwise the two-octet field. The hand-rolled parser this replaced read the
        // two-octet field only, so a four-octet peer was recorded as AS 23456 (AS_TRANS).
        let peer_as = open.my_asn4();
        if peer_as == 0 {
            Self::send_notification(write_half, wire::ERR_OPEN, wire::SUB_BAD_PEER_AS, &[]).await;
            bail!("BGP peer advertised AS 0");
        }

        let peer_hold = open.hold_time();
        if peer_hold != 0 && peer_hold < MIN_HOLD_TIME {
            Self::send_notification(
                write_half,
                wire::ERR_OPEN,
                wire::SUB_UNACCEPTABLE_HOLD_TIME,
                &[],
            )
            .await;
            bail!("BGP peer proposed an unacceptable hold time of {peer_hold}s");
        }

        let peer_asn4 = open
            .capabilities()
            .into_iter()
            .any(|c| matches!(c, BgpCapability::FourOctetAs(_)));
        let peer_router_id = open.bgp_id().to_string();

        let negotiated = {
            let mut data = client_data.lock().await;
            if data.bgp_state != BgpState::OpenSent {
                warn!(
                    "BGP client {} received a second OPEN in state {:?}, ignoring",
                    client_id, data.bgp_state
                );
                return Ok(());
            }
            data.peer_as = Some(peer_as);
            data.peer_router_id = Some(peer_router_id.clone());
            data.peer_asn4 = peer_asn4;
            data.negotiated_hold_time = negotiate_hold(data.proposed_hold_time, peer_hold);
            data.bgp_state = BgpState::OpenConfirm;
            data.negotiated_hold_time
        };

        info!(
            "BGP client {} received OPEN: AS={}, hold_time={}s, router_id={}, asn4={}, negotiated hold {}s",
            client_id, peer_as, peer_hold, peer_router_id, peer_asn4, negotiated
        );
        let _ = status_tx.send(format!(
            "[CLIENT] BGP OPEN received: AS={peer_as}, hold_time={peer_hold}s, router_id={peer_router_id}"
        ));

        // RFC 4271 section 8.2.2: having sent our OPEN and received theirs, send a KEEPALIVE
        // and enter OpenConfirm. Established is reached only when the peer's KEEPALIVE
        // arrives; the previous implementation declared Established here, before the peer had
        // confirmed anything.
        write_half
            .lock()
            .await
            .write_all(&wire::encode_keepalive())
            .await
            .context("Failed to send BGP KEEPALIVE")?;

        info!(
            "BGP client {} sent KEEPALIVE, entering OpenConfirm",
            client_id
        );
        let _ = status_tx.send("[CLIENT] BGP KEEPALIVE sent".to_string());

        Ok(())
    }

    /// Handle a KEEPALIVE. The hold timer has already been reset by the caller, which is the
    /// entire semantics of a KEEPALIVE; the only other thing one can do is complete the
    /// handshake the first time it arrives.
    #[allow(clippy::too_many_arguments)]
    async fn handle_keepalive_message(
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        write_half: &SharedWriter,
        shutdown: &Arc<Shutdown>,
    ) -> Result<()> {
        debug!("BGP client {} received KEEPALIVE", client_id);

        let (became_established, peer_as, peer_router_id, hold_time, peer_asn4) = {
            let mut data = client_data.lock().await;
            let became = data.bgp_state == BgpState::OpenConfirm;
            if became {
                data.bgp_state = BgpState::Established;
            }
            (
                became,
                data.peer_as,
                data.peer_router_id.clone(),
                data.negotiated_hold_time,
                data.peer_asn4,
            )
        };

        if !became_established {
            let _ = status_tx.send("[CLIENT] BGP KEEPALIVE received".to_string());
            return Ok(());
        }

        info!("BGP client {} session established", client_id);
        let _ = status_tx.send("[CLIENT] BGP session established".to_string());

        let remote_addr = app_state
            .get_client(client_id)
            .await
            .map(|c| c.remote_addr)
            .unwrap_or_default();

        let event = Event::new(
            &BGP_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "remote_addr": remote_addr,
                "peer_as": peer_as,
                "peer_router_id": peer_router_id,
                "hold_time": hold_time,
                "peer_supports_four_octet_as": peer_asn4,
            }),
        );

        Self::dispatch_event(
            &event,
            client_id,
            llm_client,
            app_state,
            status_tx,
            client_data,
            Some(write_half),
            shutdown,
        )
        .await;

        Ok(())
    }

    /// Handle a peer UPDATE: report the decoded routes to the handler.
    ///
    /// The handler *does* get a reply path. It used to be passed `None`, on the reasoning that
    /// "this client announces nothing, so an UPDATE handler cannot reply on the wire" — but that
    /// conflates announcing routes with answering at all. This client's whole vocabulary is
    /// `send_keepalive` / `send_notification` / `disconnect` / `wait_for_more`; none of them is
    /// a route announcement, and a monitor that sees a prefix it must not accept wanting to tear
    /// the session down is the obvious use for the event. So every action the model returned was
    /// executed nowhere and logged as discarded, which is the "asks the model and throws the
    /// answer away" defect the root CLAUDE.md describes, softened by a warning.
    ///
    /// `bgp_notification_received` keeps `None`, and that one is genuinely correct: RFC 4271
    /// section 4.5 forbids answering a NOTIFICATION at all.
    #[allow(clippy::too_many_arguments)]
    async fn handle_update_message(
        update: &netgauze_bgp_pkt::update::BgpUpdateMessage,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        write_half: &SharedWriter,
        shutdown: &Arc<Shutdown>,
    ) {
        info!(
            "BGP client {} received UPDATE: {} withdrawn, {} announced",
            client_id,
            update.withdraw_routes().len(),
            update.nlri().len()
        );
        let _ = status_tx.send(format!(
            "[CLIENT] BGP UPDATE received: {} withdrawn, {} announced",
            update.withdraw_routes().len(),
            update.nlri().len()
        ));

        // Structured, one field per concept. This used to be `hex::encode(body)`, which the
        // root CLAUDE.md forbids and which no model can act on.
        let mut data = wire::update_to_json(update);
        data["peer_as"] = serde_json::json!(client_data.lock().await.peer_as);

        let event = Event::new(&BGP_CLIENT_UPDATE_RECEIVED_EVENT, data);

        Self::dispatch_event(
            &event,
            client_id,
            llm_client,
            app_state,
            status_tx,
            client_data,
            Some(write_half),
            shutdown,
        )
        .await;
    }

    /// Handle a peer NOTIFICATION. The session is over; nothing is written back.
    #[allow(clippy::too_many_arguments)]
    async fn handle_notification_message(
        error_code: u8,
        error_subcode: u8,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        shutdown: &Arc<Shutdown>,
    ) {
        let name = wire::error_name(error_code);
        let subcode_name = wire::error_subcode_name(error_code, error_subcode);

        error!(
            "BGP client {} received NOTIFICATION: {} / {} (code={}, subcode={})",
            client_id, name, subcode_name, error_code, error_subcode
        );
        let _ = status_tx.send(format!(
            "[CLIENT] BGP NOTIFICATION: {name} / {subcode_name}"
        ));

        let event = Event::new(
            &BGP_CLIENT_NOTIFICATION_RECEIVED_EVENT,
            serde_json::json!({
                "error_code": error_code,
                "error_name": name,
                "error_subcode": error_subcode,
                "error_subcode_name": subcode_name,
                "peer_as": client_data.lock().await.peer_as,
            }),
        );

        Self::dispatch_event(
            &event,
            client_id,
            llm_client,
            app_state,
            status_tx,
            client_data,
            None,
            shutdown,
        )
        .await;
    }

    /// Run the handler for `event`, update memory, and execute whatever it returned.
    ///
    /// The memory string is copied out and the guard dropped *before* the LLM call. Holding it
    /// across the await was a deadlock, not just a style violation: the success arm reacquired
    /// the same non-reentrant `tokio::sync::Mutex` to store `memory_updates`, so any handler
    /// that updated memory hung the client's read loop forever.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_event(
        event: &Event,
        client_id: ClientId,
        llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        client_data: &Arc<Mutex<ClientData>>,
        write_half: Option<&SharedWriter>,
        shutdown: &Arc<Shutdown>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let memory = client_data.lock().await.memory.clone();
        let protocol = BgpClientProtocol::new();

        match call_llm_for_client(
            llm_client,
            app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(event),
            &protocol,
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
                if let Some(write_half) = write_half {
                    Self::execute_actions(actions, write_half, client_id, &protocol, shutdown)
                        .await;
                } else if !actions.is_empty() {
                    // Silently dropping them would look like they had been sent.
                    warn!(
                        "BGP client {} discarded {} action(s) for event '{}': this event has no \
                         reply path on the wire",
                        client_id,
                        actions.len(),
                        event.id()
                    );
                }
            }
            Err(e) => {
                error!("LLM error for BGP client {}: {}", client_id, e);
            }
        }
    }

    /// Send a NOTIFICATION, best-effort: the session is being torn down either way.
    async fn send_notification(write_half: &SharedWriter, code: u8, subcode: u8, data: &[u8]) {
        match wire::encode_notification(code, subcode, data) {
            Ok(bytes) => {
                let _ = write_half.lock().await.write_all(&bytes).await;
            }
            Err(e) => error!("BGP client could not encode NOTIFICATION {code}/{subcode}: {e}"),
        }
    }

    /// Keepalive cadence and hold-timer enforcement, in one task.
    ///
    /// Two RFC 4271 obligations, both on the same clock:
    ///
    /// * section 10 — KeepaliveTime is a third of the negotiated HoldTime, at least one second.
    ///   Without this the client sent a KEEPALIVE only in reply to one, so any peer enforcing
    ///   its hold timer dropped the session after `hold_time` seconds of silence.
    /// * sections 4.2 and 6.5 — when the Hold Timer expires, the speaker MUST send a
    ///   NOTIFICATION with error code 4 (Hold Timer Expired) and close the session. Until this
    ///   existed a silent peer was noticed only when TCP eventually noticed, which can be many
    ///   minutes and, on a half-open connection with no traffic, never.
    ///
    /// It runs in its own task for the same reason the server's does: the read loop can be
    /// parked in an LLM call, and a hold timer that only ticks between model calls is not a
    /// timer. Expiry raises `shutdown`, which the read loop selects on, so the teardown happens
    /// even mid-read.
    ///
    /// A zero hold time disables both timers (RFC 4271 section 4.2), so nothing is scheduled —
    /// spinning a zero-second ticker would busy-loop and expire instantly.
    #[allow(clippy::too_many_arguments)]
    fn spawn_timers(
        write_half: SharedWriter,
        hold_time: u16,
        client_id: ClientId,
        last_received: Arc<AtomicU64>,
        started: std::time::Instant,
        shutdown: Arc<Shutdown>,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if hold_time == 0 {
            debug!(
                "BGP client {} negotiated hold time 0: keepalive and hold timers disabled",
                client_id
            );
            return None;
        }
        let interval = std::time::Duration::from_secs(u64::from(hold_time).div_ceil(3).max(1));
        Some(tokio::spawn(async move {
            let keepalive = wire::encode_keepalive();
            loop {
                tokio::time::sleep(interval).await;

                let silent = started
                    .elapsed()
                    .as_secs()
                    .saturating_sub(last_received.load(Ordering::Relaxed));
                if silent >= u64::from(hold_time) {
                    error!(
                        "BGP client {} hold timer expired after {}s of silence (hold {}s)",
                        client_id, silent, hold_time
                    );
                    let _ = status_tx.send(format!(
                        "[CLIENT] BGP hold timer expired after {silent}s of silence, closing session"
                    ));
                    // Written from here rather than signalled to the read loop: the read loop
                    // may be inside an LLM call, and RFC 4271 wants the NOTIFICATION out before
                    // the connection goes away.
                    match wire::encode_notification(wire::ERR_HOLD_TIMER_EXPIRED, 0, &[]) {
                        Ok(bytes) => {
                            let _ = write_half.lock().await.write_all(&bytes).await;
                        }
                        Err(e) => {
                            error!("BGP client {client_id} could not encode NOTIFICATION 4/0: {e}")
                        }
                    }
                    shutdown.set();
                    return;
                }

                if write_half.lock().await.write_all(&keepalive).await.is_err() {
                    debug!(
                        "BGP client {} keepalive task stopping: write failed",
                        client_id
                    );
                    return;
                }
                trace!("BGP client {} sent KEEPALIVE", client_id);
            }
        }))
    }

    /// Execute actions from LLM
    async fn execute_actions(
        actions: Vec<serde_json::Value>,
        write_half: &SharedWriter,
        client_id: ClientId,
        protocol: &dyn Client,
        shutdown: &Arc<Shutdown>,
    ) {
        for action in actions {
            match protocol.execute_action(action) {
                Ok(result) => {
                    if Self::apply_result(result, write_half, client_id, shutdown).await {
                        break;
                    }
                }
                Err(e) => {
                    error!("BGP client {} action error: {}", client_id, e);
                }
            }
        }
    }

    /// Apply one action result. Returns `true` when the session should stop processing.
    async fn apply_result(
        result: ClientActionResult,
        write_half: &SharedWriter,
        client_id: ClientId,
        shutdown: &Arc<Shutdown>,
    ) -> bool {
        match result {
            ClientActionResult::SendData(bytes) => {
                match write_half.lock().await.write_all(&bytes).await {
                    Ok(()) => trace!("BGP client {} sent {} bytes", client_id, bytes.len()),
                    Err(e) => error!("BGP client {} write failed: {}", client_id, e),
                }
                false
            }
            ClientActionResult::Disconnect => {
                info!("BGP client {} disconnecting", client_id);
                // Stopping the action loop is not stopping the session: without this the read
                // loop went straight back to `read_exact` and the `disconnect` action's own
                // description ("sends a Cease NOTIFICATION, then closes") was half true.
                shutdown.set();
                true
            }
            ClientActionResult::WaitForMore | ClientActionResult::NoAction => false,
            // `disconnect` is a Multiple: a Cease NOTIFICATION followed by the close. The
            // previous implementation logged this variant and discarded it, so the graceful
            // shutdown its own action description promised never reached the wire.
            ClientActionResult::Multiple(results) => {
                for inner in results {
                    if Box::pin(Self::apply_result(inner, write_half, client_id, shutdown)).await {
                        return true;
                    }
                }
                false
            }
            ClientActionResult::Custom { name, .. } => {
                debug!("BGP client {} custom action: {}", client_id, name);
                false
            }
        }
    }
}
