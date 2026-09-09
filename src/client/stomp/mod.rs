//! STOMP 1.2 client implementation.
//!
//! # The handshake is a gate, not a formality
//!
//! `connect()` does not return until the broker has answered `CONNECT` with a well-formed
//! `CONNECTED` carrying a `version` header naming 1.2. Anything else — an `ERROR`, a
//! `CONNECTED` with no version, a version we did not offer, some other command, silence until
//! the timeout, or a closed socket — is an `Err`, so `client_startup` records
//! [`ClientStatus::Error`] and no `stomp_connected` event is ever raised. This is exactly the
//! strictness `async-stomp` applies to *our* server (its `Connector::connect()` refuses to
//! hand back a transport otherwise), and it is the difference between a client that reports a
//! session and a client that has one.
//!
//! # No per-connection state machine, deliberately
//!
//! `src/server/tcp/mod.rs` carries Idle/Processing/Accumulating because raw TCP has no frame
//! boundaries, so a second read arriving mid-LLM-call has to be queued somewhere. STOMP *has*
//! frame boundaries: this loop reads, parses out whole frames, and handles them one at a time
//! in the same task. Bytes arriving during a model call sit in the socket buffer — TCP's own
//! backpressure — and are parsed on the next pass. There is no window in which two model calls
//! could overlap on this connection, so there is nothing to track.
//!
//! # Why there is no boxed follow-up recursion here
//!
//! The root `CLAUDE.md` requires a depth-bounded, boxed recursive call for clients whose
//! action → event → action cycle is self-referential in-process. This client's cycle is not:
//! every action it can take puts a frame on the wire, and the answer comes back as another
//! frame that the same read loop parses and reports. The chain continues by itself, through
//! the socket, exactly as `datalink`'s does through its pcap queue — so the model can
//! subscribe, be told about the `MESSAGE` that arrives, and publish in response, without any
//! function here calling itself. The only bound needed is the one every client already has,
//! `client/llm_budget.rs`.
//!
//! # Graceful shutdown
//!
//! The `disconnect` action does not hang up. It writes `DISCONNECT` with a fixed `receipt`
//! ([`actions::DISCONNECT_RECEIPT_ID`]) and this loop closes when the matching `RECEIPT`
//! arrives — the specification's shutdown, and the only thing that tells a publisher the
//! broker durably took what it sent. A broker that never answers is bounded by
//! [`DISCONNECT_GRACE`] rather than leaving the loop parked forever.

pub mod actions;

pub use actions::StompClientProtocol;

use anyhow::{anyhow, bail, Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::stomp::actions::{
    DISCONNECT_RECEIPT_ID, STOMP_ACCEPT_VERSION, STOMP_CLIENT_CONNECTED_EVENT,
    STOMP_CLIENT_ERROR_RECEIVED_EVENT, STOMP_CLIENT_MESSAGE_RECEIVED_EVENT,
    STOMP_CLIENT_RECEIPT_RECEIVED_EVENT, STOMP_HEARTBEAT,
};
use crate::llm::actions::client_trait::Client;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::{Event, StartupParams};
use crate::server::stomp::frame::{self, ParseOutcome, StompFrame};
use crate::state::app_state::AppState;
use crate::state::{ClientId, ClientStatus};

/// How long to wait for the broker's `CONNECTED` when the caller declares no preference.
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 20;

/// How long to wait for the `RECEIPT` acknowledging our own `DISCONNECT` before closing anyway.
///
/// A broker is entitled to close the socket the moment it sees `DISCONNECT` without ever
/// sending the receipt, and some do. Without this bound the loop would sit on a connection
/// nobody is going to speak on again.
const DISCONNECT_GRACE: Duration = Duration::from_secs(10);

/// STOMP 1.2 client.
pub struct StompClient;

/// What the loop should do after handling a frame or a batch of actions.
enum Flow {
    /// Keep reading.
    Continue,
    /// A `DISCONNECT` was written; close once its `RECEIPT` arrives or the grace period lapses.
    DisconnectSent,
    /// The session is over now.
    Close,
}

impl StompClient {
    /// Open a STOMP session and drive it from the model.
    ///
    /// Returns the local socket address once the handshake has completed and been validated.
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        // Every accessor is fallible and every failure names the offending key: these values
        // come from the model or an MCP caller, so `?` rather than `unwrap()`.
        let (host_param, login, passcode, use_stomp_command, handshake_timeout_secs) =
            match &startup_params {
                Some(params) => (
                    params.get_optional_string("host")?,
                    params.get_optional_string("login")?,
                    params.get_optional_string("passcode")?,
                    params
                        .get_optional_bool("use_stomp_command")?
                        .unwrap_or(false),
                    params
                        .get_optional_u64("handshake_timeout_secs")?
                        .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_SECS),
                ),
                None => (None, None, None, false, DEFAULT_HANDSHAKE_TIMEOUT_SECS),
            };

        let stream = TcpStream::connect(&remote_addr)
            .await
            .with_context(|| format!("Failed to connect to STOMP broker at {remote_addr}"))?;
        let local_addr = stream.local_addr()?;
        let peer_addr = stream.peer_addr()?;

        let (mut read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(Mutex::new(write_half));

        // The specification says a client sends the host name it established the socket to
        // when it has nothing better; the virtual host is the "better" a broker configuration
        // supplies.
        let host = host_param.unwrap_or_else(|| host_of(&remote_addr));

        // CONNECT is exempt from STOMP 1.2 header escaping (see `frame::should_escape`), so
        // these three values reach the wire raw and nothing downstream can neutralise a
        // newline in one. A `login` containing `\n` would forge an extra header on the
        // handshake frame, and `\n\n` would end the header block. They come from startup
        // parameters, which is the model or an MCP caller, so they are checked rather than
        // trusted. Rejecting rather than escaping: a 1.0/1.1 broker would read an escape
        // sequence literally, which is the whole reason the command is exempt.
        for (field, value) in [
            ("host", Some(host.as_str())),
            ("login", login.as_deref()),
            ("passcode", passcode.as_deref()),
        ] {
            if let Some(value) = value {
                if !crate::server::stomp::frame::is_safe_unescaped_header(value) {
                    anyhow::bail!(
                        "STOMP startup parameter '{field}' may not contain a newline, carriage \
                         return, colon or NUL: CONNECT headers are exempt from 1.2 escaping, so \
                         such a value would forge a header rather than appear in this one"
                    );
                }
            }
        }

        let connect_frame = build_connect_frame(
            use_stomp_command,
            &host,
            login.as_deref(),
            passcode.as_deref(),
        );
        {
            let mut guard = write_half.lock().await;
            guard
                .write_all(&connect_frame)
                .await
                .context("Failed to write the STOMP CONNECT frame")?;
            guard.flush().await.context("Failed to flush CONNECT")?;
        }
        debug!(
            "STOMP client {} sent {} to {} (host={}, accept-version={}, heart-beat={})",
            client_id,
            if use_stomp_command {
                "STOMP"
            } else {
                "CONNECT"
            },
            peer_addr,
            host,
            STOMP_ACCEPT_VERSION,
            STOMP_HEARTBEAT
        );

        // Anything left in the buffer after the handshake belongs to the session — a broker is
        // free to pipeline a MESSAGE immediately behind CONNECTED, and dropping the remainder
        // would lose it.
        let mut pending: Vec<u8> = Vec::new();
        let reply = read_one_frame(
            &mut read_half,
            &mut pending,
            Duration::from_secs(handshake_timeout_secs),
        )
        .await
        .context("STOMP handshake failed")?;

        let (version, session, server) = validate_connected(&reply)?;

        info!(
            "STOMP client {} connected to {} (version={}, session={}, server={})",
            client_id,
            peer_addr,
            version,
            if session.is_empty() {
                "-"
            } else {
                session.as_str()
            },
            if server.is_empty() {
                "-"
            } else {
                server.as_str()
            }
        );
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] STOMP client {client_id} connected to {peer_addr} (session {})",
            if session.is_empty() {
                "-"
            } else {
                session.as_str()
            }
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Registered BEFORE the connected event is handled: a `manual` routing rule can park
        // that event at the dashboard for minutes, and until registration the UI reports "no
        // command channel" — which reads as a protocol limitation when it is only a queue.
        let mut command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        let protocol = StompClientProtocol::new();
        let memory = Arc::new(Mutex::new(
            app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default(),
        ));

        let mut disconnect_deadline: Option<tokio::time::Instant> = None;

        let connected_event = Event::new(
            &STOMP_CLIENT_CONNECTED_EVENT,
            serde_json::json!({
                "version": version,
                "session": session,
                "server": server,
            }),
        );
        match report_event(
            connected_event,
            &protocol,
            &write_half,
            &llm_client,
            &app_state,
            &status_tx,
            client_id,
            &memory,
        )
        .await
        {
            Flow::Close => {
                // Nothing to unwind: the loop has not started, so close here and report the
                // session as over rather than spawning a task with nothing to do.
                let _ = write_half.lock().await.shutdown().await;
                app_state.remove_client_handle(client_id).await;
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                return Ok(local_addr);
            }
            Flow::DisconnectSent => {
                disconnect_deadline = Some(tokio::time::Instant::now() + DISCONNECT_GRACE);
            }
            Flow::Continue => {}
        }

        // Registered with AppState so stop_client can abort it — dropping a JoinHandle only
        // detaches the task in Tokio, and an un-aborted read loop keeps the socket alive after
        // the client has been removed.
        let task_registrar = app_state.clone();
        let task_handle = tokio::spawn(async move {
            run_session(
                read_half,
                write_half,
                pending,
                &mut command_rx,
                disconnect_deadline,
                protocol,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                client_id,
                memory,
            )
            .await;

            // Every exit lands here. The loop owns the only receiver, so dropping the
            // registered handle makes a later send_to_client fail fast instead of timing out.
            app_state.remove_client_handle(client_id).await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(local_addr)
    }
}

/// The read loop: parse frames, report each to the model, write whatever it answers with.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    mut read_half: tokio::io::ReadHalf<TcpStream>,
    write_half: Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    mut pending: Vec<u8>,
    command_rx: &mut mpsc::Receiver<crate::state::client_handles::ClientCommand>,
    mut disconnect_deadline: Option<tokio::time::Instant>,
    protocol: StompClientProtocol,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    client_id: ClientId,
    memory: Arc<Mutex<String>>,
) {
    let mut read_buf = vec![0u8; 8192];

    'session: loop {
        // Drain every complete frame already buffered before asking the socket for more: a
        // single read routinely carries several, and a broker that pipelines must not have its
        // later frames wait for its next write.
        loop {
            let outcome = match frame::parse_frame(&pending) {
                Ok(outcome) => outcome,
                Err(e) => {
                    // STOMP cannot resynchronise a stream once framing is lost, so this is
                    // fatal for the connection rather than something to skip past.
                    error!("STOMP client {client_id} could not parse the broker's frame: {e}");
                    let _ = status_tx.send(format!(
                        "[CLIENT] ✖ STOMP client {client_id} framing error: {e}"
                    ));
                    break 'session;
                }
            };
            match outcome {
                ParseOutcome::Incomplete => break,
                ParseOutcome::Heartbeat { consumed } => {
                    trace!("STOMP client {client_id} drained {consumed} inter-frame EOL byte(s)");
                    pending.drain(..consumed);
                }
                ParseOutcome::Frame { frame, consumed } => {
                    pending.drain(..consumed);
                    match handle_frame(
                        frame,
                        &protocol,
                        &write_half,
                        &llm_client,
                        &app_state,
                        &status_tx,
                        client_id,
                        &memory,
                    )
                    .await
                    {
                        Flow::Continue => {}
                        Flow::DisconnectSent => {
                            disconnect_deadline =
                                Some(tokio::time::Instant::now() + DISCONNECT_GRACE);
                        }
                        Flow::Close => break 'session,
                    }
                }
            }
        }

        let grace = async {
            match disconnect_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            // `AsyncReadExt::read` is cancellation-safe, so losing this branch to another arm
            // does not drop bytes.
            read = read_half.read(&mut read_buf) => match read {
                Ok(0) => {
                    info!("STOMP client {client_id} — broker closed the connection");
                    break 'session;
                }
                Ok(n) => {
                    trace!("STOMP client {client_id} read {n} byte(s)");
                    pending.extend_from_slice(&read_buf[..n]);
                }
                Err(e) => {
                    error!("STOMP client {client_id} read error: {e}");
                    app_state
                        .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                        .await;
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                    return;
                }
            },

            Some(command) = command_rx.recv() => {
                // Peeked before the command is moved into the generic arm, which writes the
                // frame but cannot know what it means. `disconnect` produces a DISCONNECT
                // frame rather than a socket close, so the loop has to start the grace timer
                // itself — for an injected action exactly as for a model-produced one.
                let is_disconnect = command
                    .action
                    .get("type")
                    .and_then(|v| v.as_str())
                    == Some("disconnect");

                let should_break = crate::client::command_support::handle_stream_client_command(
                    &protocol,
                    &write_half,
                    command,
                    client_id,
                    &app_state,
                    &status_tx,
                )
                .await;

                if is_disconnect {
                    disconnect_deadline = Some(tokio::time::Instant::now() + DISCONNECT_GRACE);
                }
                if should_break {
                    break 'session;
                }
            }

            _ = grace => {
                warn!(
                    "STOMP client {client_id} closing without the RECEIPT for its DISCONNECT \
                     ({}s grace elapsed) — the broker acknowledged nothing",
                    DISCONNECT_GRACE.as_secs()
                );
                break 'session;
            }
        }
    }

    let _ = write_half.lock().await.shutdown().await;
    app_state
        .update_client_status(client_id, ClientStatus::Disconnected)
        .await;
    let _ = status_tx.send(format!("[CLIENT] STOMP client {client_id} disconnected"));
}

/// Turn one inbound frame into an event, ask the model, and run its answer.
#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    frame: StompFrame,
    protocol: &StompClientProtocol,
    write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    client_id: ClientId,
    memory: &Arc<Mutex<String>>,
) -> Flow {
    match frame.command.as_str() {
        "MESSAGE" => {
            let (body, body_encoding) = body_for_event(&frame.body);
            let event = Event::new(
                &STOMP_CLIENT_MESSAGE_RECEIVED_EVENT,
                serde_json::json!({
                    "destination": frame.header("destination").unwrap_or_default(),
                    "message_id": frame.header("message-id").unwrap_or_default(),
                    "subscription": frame.header("subscription").unwrap_or_default(),
                    "headers": frame.headers_except(&["destination", "message-id", "subscription"]),
                    "body": body,
                    "body_encoding": body_encoding,
                }),
            );
            debug!(
                "STOMP client {client_id} received MESSAGE on {} (subscription {})",
                frame.header("destination").unwrap_or("-"),
                frame.header("subscription").unwrap_or("-")
            );
            report_event(
                event, protocol, write_half, llm_client, app_state, status_tx, client_id, memory,
            )
            .await
        }

        "RECEIPT" => {
            let receipt_id = frame.header("receipt-id").unwrap_or_default().to_string();

            // The acknowledgement of our own DISCONNECT ends the session. Reporting it to the
            // model would spend a call on a connection whose answer could not be written
            // anywhere; the log records it instead.
            if receipt_id == DISCONNECT_RECEIPT_ID {
                info!(
                    "STOMP client {client_id} — broker acknowledged DISCONNECT, closing \
                     gracefully"
                );
                return Flow::Close;
            }

            debug!("STOMP client {client_id} received RECEIPT {receipt_id}");
            let event = Event::new(
                &STOMP_CLIENT_RECEIPT_RECEIVED_EVENT,
                serde_json::json!({ "receipt_id": receipt_id }),
            );
            report_event(
                event, protocol, write_half, llm_client, app_state, status_tx, client_id, memory,
            )
            .await
        }

        "ERROR" => {
            let message = frame.header("message").unwrap_or_default().to_string();
            let (body, body_encoding) = body_for_event(&frame.body);
            warn!("STOMP client {client_id} received ERROR from the broker: {message}");
            let _ = status_tx.send(format!(
                "[CLIENT] ⚠ STOMP client {client_id} got an ERROR frame: {message}"
            ));
            let event = Event::new(
                &STOMP_CLIENT_ERROR_RECEIVED_EVENT,
                serde_json::json!({
                    "message": message,
                    "body": body,
                    "body_encoding": body_encoding,
                }),
            );
            // Reported so the model learns what happened and can record it, then closed
            // regardless of what it answers: the specification has the broker close the
            // connection immediately after an ERROR, so nothing written here would arrive.
            let _ = report_event(
                event, protocol, write_half, llm_client, app_state, status_tx, client_id, memory,
            )
            .await;
            Flow::Close
        }

        "CONNECTED" => {
            // A second CONNECTED mid-session is not defined. Ignoring it is safer than
            // re-running the handshake, and it is not fatal the way a framing error is.
            warn!("STOMP client {client_id} ignoring an unexpected second CONNECTED frame");
            Flow::Continue
        }

        other => {
            warn!(
                "STOMP client {client_id} ignoring a frame with unexpected command {other:?} — \
                 a broker sends only MESSAGE, RECEIPT, ERROR and CONNECTED"
            );
            Flow::Continue
        }
    }
}

/// Ask the model about `event` and execute whatever it answers with.
#[allow(clippy::too_many_arguments)]
async fn report_event(
    event: Event,
    protocol: &StompClientProtocol,
    write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    llm_client: &OllamaClient,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    client_id: ClientId,
    memory: &Arc<Mutex<String>>,
) -> Flow {
    let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
        debug!(
            "STOMP client {client_id} has no instruction; {} is not reported to the model",
            event.event_type.id
        );
        return Flow::Continue;
    };

    // Copied out and the guard dropped before the call: never hold a lock across an await that
    // performs I/O or an LLM call.
    let memory_snapshot = memory.lock().await.clone();

    match call_llm_for_client(
        llm_client,
        app_state,
        client_id.to_string(),
        &instruction,
        &memory_snapshot,
        Some(&event),
        protocol,
        status_tx,
    )
    .await
    {
        Ok(result) => {
            if let Some(updated) = result.memory_updates {
                *memory.lock().await = updated.clone();
                app_state.set_memory_for_client(client_id, updated).await;
            }
            run_actions(
                result.actions,
                protocol,
                write_half,
                app_state,
                status_tx,
                client_id,
            )
            .await
        }
        Err(e) => {
            // A STOMP client has no error frame to send: ERROR is a server frame, and inventing
            // wire traffic because our own backend failed would tell the broker something
            // untrue. The session stays open — the next delivery gets another chance — and the
            // failure is recorded in both logs.
            error!("STOMP client {client_id} LLM error on {}: {e}", event.id());
            let _ = status_tx.send(format!(
                "[CLIENT] ✖ STOMP client {client_id} could not answer {}: {e}",
                event.id()
            ));
            Flow::Continue
        }
    }
}

/// Execute the model's actions in order, writing every frame they produce.
async fn run_actions(
    actions: Vec<serde_json::Value>,
    protocol: &StompClientProtocol,
    write_half: &Arc<Mutex<tokio::io::WriteHalf<TcpStream>>>,
    app_state: &Arc<AppState>,
    status_tx: &mpsc::UnboundedSender<String>,
    client_id: ClientId,
) -> Flow {
    let mut flow = Flow::Continue;

    for action in actions {
        // `disconnect` writes a DISCONNECT frame rather than closing, so the loop learns from
        // the action's own name that a graceful shutdown is in progress.
        let is_disconnect = action.get("type").and_then(|v| v.as_str()) == Some("disconnect");

        let result = match protocol.execute_action(action.clone()) {
            Ok(result) => result,
            Err(e) => {
                warn!("STOMP client {client_id} rejected an action from the model: {e}");
                let _ = status_tx.send(format!(
                    "[CLIENT] ⚠ STOMP client {client_id} rejected an action: {e}"
                ));
                continue;
            }
        };

        for bytes in result.get_all_data() {
            let mut guard = write_half.lock().await;
            if let Err(e) = guard.write_all(&bytes).await {
                error!("STOMP client {client_id} write failed: {e}");
                return Flow::Close;
            }
            if let Err(e) = guard.flush().await {
                error!("STOMP client {client_id} flush failed: {e}");
                return Flow::Close;
            }
            drop(guard);
            trace!("STOMP client {client_id} wrote {} byte(s)", bytes.len());
        }

        if result.disconnects() {
            // Nothing in this protocol's vocabulary produces this today; it is here so a
            // future action that hangs up is honoured rather than silently ignored.
            app_state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            return Flow::Close;
        }
        if is_disconnect {
            flow = Flow::DisconnectSent;
        }
    }

    flow
}

/// Build the opening `CONNECT` (or `STOMP`) frame.
///
/// `heart-beat` is always `0,0`: this client runs no heart-beat timer, and a broker told
/// otherwise tears the connection down when the promised bare EOLs do not arrive.
fn build_connect_frame(
    use_stomp_command: bool,
    host: &str,
    login: Option<&str>,
    passcode: Option<&str>,
) -> Vec<u8> {
    let mut headers = vec![
        (
            "accept-version".to_string(),
            STOMP_ACCEPT_VERSION.to_string(),
        ),
        ("host".to_string(), host.to_string()),
        ("heart-beat".to_string(), STOMP_HEARTBEAT.to_string()),
    ];
    if let Some(login) = login {
        headers.push(("login".to_string(), login.to_string()));
    }
    if let Some(passcode) = passcode {
        headers.push(("passcode".to_string(), passcode.to_string()));
    }
    let command = if use_stomp_command {
        "STOMP"
    } else {
        "CONNECT"
    };
    StompFrame::new(command, headers, Vec::new()).encode()
}

/// Check the broker's reply to `CONNECT`, returning `(version, session, server)`.
///
/// Every failure here is a refusal to proceed. A client that treats a missing `version` as
/// "probably fine" has no idea which dialect the following frames are in, and STOMP 1.1 and
/// 1.2 disagree about the very headers this client's `ACK` uses.
fn validate_connected(reply: &StompFrame) -> Result<(String, String, String)> {
    match reply.command.as_str() {
        "CONNECTED" => {}
        "ERROR" => {
            let message = reply.header("message").unwrap_or("(no message header)");
            bail!(
                "the broker refused the session with an ERROR frame: {message}. This client \
                 offers accept-version {STOMP_ACCEPT_VERSION} and heart-beat \
                 {STOMP_HEARTBEAT}."
            );
        }
        other => bail!(
            "expected CONNECTED in reply to CONNECT, got a {other:?} frame. The session is not \
             open and no frame will be sent on it."
        ),
    }

    let Some(version) = reply.header("version") else {
        bail!(
            "the broker's CONNECTED frame carries no 'version' header, which STOMP 1.2 \
             requires. Without it there is no way to know which dialect the following frames \
             are in, so the session is refused rather than guessed at."
        );
    };
    if version != STOMP_ACCEPT_VERSION {
        bail!(
            "the broker chose STOMP version {version:?}, which this client did not offer — its \
             accept-version was {STOMP_ACCEPT_VERSION:?} and it implements nothing else. \
             Speaking a dialect we do not implement would corrupt the session rather than \
             degrade it."
        );
    }

    Ok((
        version.to_string(),
        reply.header("session").unwrap_or_default().to_string(),
        reply.header("server").unwrap_or_default().to_string(),
    ))
}

/// Read frames into `pending` until one parses, or the deadline passes.
///
/// Used for the handshake only. Bytes past the frame stay in `pending` — a broker may pipeline
/// a `MESSAGE` immediately behind `CONNECTED`, and discarding the remainder would lose it.
async fn read_one_frame<R>(
    reader: &mut R,
    pending: &mut Vec<u8>,
    timeout: Duration,
) -> Result<StompFrame>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];

    loop {
        match frame::parse_frame(pending).map_err(|e| anyhow!("{e}"))? {
            ParseOutcome::Frame { frame, consumed } => {
                pending.drain(..consumed);
                return Ok(frame);
            }
            ParseOutcome::Heartbeat { consumed } => {
                pending.drain(..consumed);
                continue;
            }
            ParseOutcome::Incomplete => {}
        }

        let n = tokio::time::timeout_at(deadline, reader.read(&mut buf))
            .await
            .map_err(|_| {
                anyhow!(
                    "the broker sent no complete frame within {}s ({} byte(s) buffered)",
                    timeout.as_secs(),
                    pending.len()
                )
            })?
            .context("read failed while waiting for the broker's reply")?;
        if n == 0 {
            bail!(
                "the broker closed the connection without replying ({} byte(s) buffered)",
                pending.len()
            );
        }
        pending.extend_from_slice(&buf[..n]);
    }
}

/// The host part of `host:port`, which is what a STOMP `host` header defaults to.
///
/// Bracketed IPv6 literals keep their brackets stripped rather than being split on the wrong
/// colon; anything unrecognisable is returned whole, because a wrong-but-present `host` header
/// is better than an absent one that makes the broker reject the frame outright.
fn host_of(remote_addr: &str) -> String {
    let trimmed = remote_addr.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    match trimmed.rsplit_once(':') {
        Some((host, _port)) if !host.is_empty() => host.to_string(),
        _ => trimmed.to_string(),
    }
}

/// Render a frame body for an event: text when it is printable, hex when it is not.
///
/// The `body_encoding` field says which, so nothing downstream has to guess — and passing the
/// pair straight into `send_stomp_send` reproduces the exact bytes. This mirrors the server's
/// own `body_for_event`, which is a private function there and so cannot be shared.
fn body_for_event(body: &[u8]) -> (String, &'static str) {
    if body
        .iter()
        .all(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
    {
        (String::from_utf8_lossy(body).to_string(), "utf8")
    } else {
        (hex::encode(body), "hex")
    }
}
