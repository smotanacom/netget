//! Gemini server (gemini://, TLS, port 1965) — the model writes the capsule.
//!
//! One connection carries one exchange: TLS handshake, one request line (an absolute URL and
//! CRLF), one response header, a body only after a 2x, then `close_notify` and close. That
//! shape decides the whole loop:
//!
//! 1. **NetGet refuses what is not a request before the model sees it.** A URL over 1024 bytes,
//!    a relative or malformed one, a BOM, userinfo or a fragment get `59`; another scheme gets
//!    `53` (this server is not a proxy). No model call is spent on any of them.
//! 2. **Every response closes the connection**, so there is no idle phase: the two deadlines
//!    are the handshake and the request line, and both wrap reads only — a request parked for a
//!    human under a `manual` rule is not closed by either.
//! 3. **The model cannot write a header.** Its actions are rendered by [`wire`]; the loop sends
//!    the first rendered response and nothing after it.
//! 4. **Failure is a status, never silence.** Backend overload → `41` (the spec's "server
//!    unavailable due to overload"), any other backend failure → `40` (the generic temporary
//!    failure), a model that produced no response → `40`. Meta texts are fixed literals.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::TlsAcceptor;

pub use wire::{MAX_REQUEST_BYTES, MAX_URL_BYTES};

/// How long a connected peer has to complete the TLS handshake. Real clients send ClientHello
/// at once and finish in a round trip; nothing in this phase involves the model.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a peer that completed the handshake has to send its request line.
///
/// A real Gemini client sends it in the same flight as its Finished. 300 seconds is the window a
/// `manual` rule gives a human — the peer may be NetGet's own TLS client, which completes the
/// handshake and then waits for its operator to type the request.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent connections admitted before new ones are refused — the house default.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] gets: nothing, then a close. Every Gemini response is
/// sent inside TLS, and a refused peer is refused precisely so that it does not get a handshake
/// — plaintext `41` bytes here would be a malformed TLS record, not a refusal. `dot` and `doh`
/// refuse the same way for the same reason.
const CONNECTION_CAP_REFUSAL: &[u8] = b"";

/// Responses NetGet writes itself when the model cannot answer. Fixed literals: nothing
/// derived from an error can reach the peer.
const UNAVAILABLE_RESPONSE: &[u8] = b"40 request could not be processed\r\n";
const OVERLOADED_RESPONSE: &[u8] = b"41 backend at capacity, retry later\r\n";
const TOO_LONG_RESPONSE: &[u8] = b"59 Request too long\r\n";

/// After the last response, how long and how much the server keeps reading before dropping
/// the socket. Closing over unread input sends RST, which can overtake the response the client
/// has not read yet — a `59` for an over-long request, typically.
const LINGER_TIME: Duration = Duration::from_secs(2);
const LINGER_BYTES: usize = 64 * 1024;

/// The two read deadlines, resolved from startup parameters.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    pub handshake: Duration,
    pub first_byte: Duration,
}

impl Deadlines {
    pub fn new(handshake_secs: Option<u64>, first_byte_secs: Option<u64>) -> Self {
        Self {
            handshake: handshake_secs
                .map(Duration::from_secs)
                .unwrap_or(HANDSHAKE_TIMEOUT),
            first_byte: first_byte_secs
                .map(Duration::from_secs)
                .unwrap_or(FIRST_BYTE_TIMEOUT),
        }
    }
}

pub struct GeminiServer;

impl GeminiServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        tls_config: Arc<rustls::ServerConfig>,
        deadlines: Deadlines,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("Gemini server listening on {}", local_addr));

        let protocol = Arc::new(actions::GeminiProtocol::new());
        let acceptor = TlsAcceptor::from(tls_config);
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "Gemini",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((socket, peer_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = crate::utils::clock::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
                                    id: connection_id,
                                    remote_addr: peer_addr,
                                    local_addr,
                                    bytes_sent: 0,
                                    bytes_received: 0,
                                    packets_sent: 0,
                                    packets_received: 0,
                                    last_activity: now,
                                    status: ConnectionStatus::Active,
                                    status_changed_at: now,
                                    protocol_info: ProtocolConnectionInfo::empty(),
                                },
                            )
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let session = Session {
                            peer_addr,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id,
                            protocol: protocol.clone(),
                            connection_id,
                            deadlines,
                        };
                        let acceptor = acceptor.clone();
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Released when the connection ends; this task is the whole
                                // connection, so MAX_CONNECTIONS caps live connections.
                                let _permit = permit;
                                session.run(acceptor, socket).await
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("Gemini accept error: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;
        Ok(local_addr)
    }
}

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::GeminiProtocol>,
    connection_id: ConnectionId,
    deadlines: Deadlines,
}

/// What reading the request line produced.
enum RequestRead {
    Line(String, usize),
    /// The line exceeded [`MAX_REQUEST_BYTES`] including its CRLF.
    TooLong,
    TimedOut,
    Closed,
}

impl Session {
    async fn run(self, acceptor: TlsAcceptor, socket: tokio::net::TcpStream) {
        let log = Log::new(Some(&self.status_tx));
        let tls =
            match tokio::time::timeout(self.deadlines.handshake, acceptor.accept(socket)).await {
                Err(_) => {
                    log.info(format!(
                        "Gemini peer {} did not complete the TLS handshake within {}s; closing",
                        self.peer_addr,
                        self.deadlines.handshake.as_secs()
                    ));
                    self.finish().await;
                    return;
                }
                Ok(Err(e)) => {
                    log.warn(format!(
                        "Gemini TLS handshake with {} failed: {}",
                        self.peer_addr, e
                    ));
                    self.finish().await;
                    return;
                }
                Ok(Ok(tls)) => tls,
            };
        log.info(format!("Gemini client connected from {}", self.peer_addr));

        let (mut reader, write_half) = tokio::io::split(tls);
        let write_half = Arc::new(Mutex::new(write_half));

        // Registered once the handshake is done — before it there is no channel a reply could
        // travel on — and before the request is read, so the operator can answer a peer whose
        // request is parked for them.
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &self.app_state,
            self.server_id,
            self.connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            self.protocol.clone(),
            self.app_state.clone(),
            self.server_id,
            self.connection_id.as_u32(),
            write_half.clone(),
            self.status_tx.clone(),
        );

        self.exchange(&mut reader, &write_half).await;

        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        // close_notify, then drain what the peer already sent so the TCP close is a FIN.
        let _ = write_half.lock().await.shutdown().await;
        let deadline = tokio::time::Instant::now() + LINGER_TIME;
        let mut sink = [0u8; 4096];
        let mut drained = 0usize;
        while drained < LINGER_BYTES {
            match tokio::time::timeout_at(deadline, reader.read(&mut sink)).await {
                Ok(Ok(n)) if n > 0 => drained += n,
                _ => break,
            }
        }
        self.finish().await;
    }

    async fn finish(&self) {
        self.app_state
            .update_connection_status(
                self.server_id,
                self.connection_id,
                crate::state::server::ConnectionStatus::Closed,
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
    }

    async fn write<W>(&self, write_half: &Arc<Mutex<W>>, data: &[u8]) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        {
            let mut writer = write_half.lock().await;
            writer.write_all(data).await?;
            writer.flush().await?;
        }
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                None,
                Some(data.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    /// Read one request line: up to CRLF (or a bare LF), bounded in size and in time.
    async fn read_request<R: tokio::io::AsyncRead + Unpin>(&self, reader: &mut R) -> RequestRead {
        let mut pending: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            if let Some(idx) = pending.iter().position(|b| *b == b'\n') {
                if idx + 1 > MAX_REQUEST_BYTES {
                    return RequestRead::TooLong;
                }
                let line = String::from_utf8_lossy(&pending[..idx]);
                return RequestRead::Line(line.trim_end_matches('\r').to_string(), idx + 1);
            }
            if pending.len() >= MAX_REQUEST_BYTES {
                return RequestRead::TooLong;
            }
            match tokio::time::timeout(self.deadlines.first_byte, reader.read(&mut chunk)).await {
                Err(_) => return RequestRead::TimedOut,
                Ok(Ok(0)) | Ok(Err(_)) => return RequestRead::Closed,
                Ok(Ok(n)) => pending.extend_from_slice(&chunk[..n]),
            }
        }
    }

    async fn exchange<R, W>(&self, reader: &mut R, write_half: &Arc<Mutex<W>>)
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let log = Log::new(Some(&self.status_tx));
        let (line, n) = match self.read_request(reader).await {
            RequestRead::Line(line, n) => (line, n),
            RequestRead::TooLong => {
                log.warn(format!(
                    "Gemini request from {} exceeded {} bytes decision=fail_closed_request_too_long",
                    self.peer_addr, MAX_REQUEST_BYTES
                ));
                let _ = self.write(write_half, TOO_LONG_RESPONSE).await;
                return;
            }
            RequestRead::TimedOut => {
                log.info(format!(
                    "Gemini peer {} sent no request within {}s; closing",
                    self.peer_addr,
                    self.deadlines.first_byte.as_secs()
                ));
                return;
            }
            RequestRead::Closed => {
                log.info(format!(
                    "Gemini peer {} closed without a request",
                    self.peer_addr
                ));
                return;
            }
        };
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                Some(n as u64),
                None,
                Some(1),
                None,
            )
            .await;

        let request = match wire::parse_request(&line) {
            Ok(request) => request,
            Err(refusal) => {
                log.info(format!(
                    "Gemini request from {} refused decision={}",
                    self.peer_addr,
                    refusal.decision()
                ));
                let _ = self.write(write_half, refusal.response().as_bytes()).await;
                return;
            }
        };

        let event = Event::new(
            &actions::GEMINI_REQUEST_EVENT,
            serde_json::json!({
                "url": request.url,
                "host": request.host,
                "path": request.path,
                "query": request.query,
            }),
        );
        let result = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                let (category, response) = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => ("overloaded", OVERLOADED_RESPONSE),
                    crate::utils::WireFailure::Unavailable => ("unavailable", UNAVAILABLE_RESPONSE),
                };
                log.warn(format!(
                    "Gemini {} from {} decision=fail_closed_llm_error category={}",
                    request.url, self.peer_addr, category
                ));
                log.debug(format!("Gemini LLM call failed: {}", e));
                let _ = self.write(write_half, response).await;
                return;
            }
        };
        for message in &result.messages {
            log.info(message);
        }

        let mut responses: Vec<Vec<u8>> = Vec::new();
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::Output(bytes) => responses.push(bytes),
                ActionResult::Multiple(items) => stack.extend(items.into_iter().rev()),
                _ => {}
            }
        }

        let Some(response) = responses.first() else {
            log.warn(format!(
                "Gemini {} from {} decision=model_silent ({} failed action(s)); answering 40",
                request.url,
                self.peer_addr,
                result.failures.len()
            ));
            let _ = self.write(write_half, UNAVAILABLE_RESPONSE).await;
            return;
        };
        if responses.len() > 1 {
            log.warn(format!(
                "Gemini {}: the model produced {} responses to one request; sending the first",
                request.url,
                responses.len()
            ));
        }
        let status = wire::leading_status(response).unwrap_or(0);
        let decision = if (10..40).contains(&status) {
            "model_answer"
        } else {
            "model_reject"
        };
        log.info(format!(
            "Gemini {} from {} decision={} status={}",
            request.url, self.peer_addr, decision, status
        ));
        if let Err(e) = self.write(write_half, response).await {
            log.debug(format!("Gemini write to {} failed: {}", self.peer_addr, e));
        }
    }
}
