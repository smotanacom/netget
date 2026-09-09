//! RTSP server (RFC 2326).
//!
//! The control front door to NetGet's RTP media. A client (ffprobe, ffplay, VLC) runs
//! OPTIONS → DESCRIBE → SETUP → PLAY → TEARDOWN; NetGet owns the RTSP framing (CSeq, Transport,
//! Session, RTP-Info) deterministically, while the model shapes the DESCRIBE SDP, decides status
//! codes, and — via PLAY — decides what the RTP stream carries. The media itself is synthesized
//! by the shared `crate::server::rtp::media` engine, so an RTSP PLAY results in real RTP arriving.

pub mod actions;

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::rtp::media::{self, AudioCodec, RtpPacketizer};
use crate::server::RtspProtocol;
use crate::state::app_state::AppState;
use actions::{
    RTSP_DESCRIBE_EVENT, RTSP_OPTIONS_EVENT, RTSP_OTHER_EVENT, RTSP_PLAY_EVENT, RTSP_SETUP_EVENT,
    RTSP_TEARDOWN_EVENT,
};

/// Parsed RTSP request.
#[derive(Debug, Clone)]
struct RtspRequest {
    method: String,
    uri: String,
    cseq: String,
    headers: HashMap<String, String>,
}

/// Per-connection RTSP session state.
#[derive(Default)]
struct Session {
    id: Option<String>,
    rtp_socket: Option<Arc<UdpSocket>>,
    client_rtp_addr: Option<SocketAddr>,
    server_rtp_port: Option<u16>,
    play_task: Option<JoinHandle<()>>,
}

pub struct RtspServer;

impl RtspServer {
    /// Spawn the RTSP server. Awaits the TCP bind so failure is reported as `Err`, and registers
    /// the accept loop so `stop_server` can abort it.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("RTSP server listening on {}", local_addr));

        let protocol = Arc::new(RtspProtocol::new());
        let task_registrar = app_state.clone();

        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm = llm_client.clone();
                        let state = app_state.clone();
                        let stx = status_tx.clone();
                        let proto = protocol.clone();
                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                remote_addr,
                                connection_id,
                                server_id,
                                llm,
                                state,
                                stx.clone(),
                                proto,
                            )
                            .await
                            {
                                Log::new(Some(&stx))
                                    .debug(format!("RTSP connection closed: {}", e));
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("RTSP accept error: {}", e));
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;
        Ok(local_addr)
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: TcpStream,
        remote_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm: OllamaClient,
        state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        protocol: Arc<RtspProtocol>,
    ) -> Result<()> {
        // Share the write half between the reader loop and the peer-injection task through
        // one Arc<Mutex<_>>, so an injected write cannot interleave with a response.
        let (read_half, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));
        let mut session = Session::default();

        // Peer messaging: register a handle so the dashboard's "message this peer" /
        // "disconnect this peer" can inject an action into THIS connection through the same
        // executor the LLM path uses. RTSP's response verbs return `NoAction` (framing — CSeq,
        // Transport, Session — is built here in mod.rs, not by the action), so an injected
        // `rtsp_*_response` writes no bytes; the meaningful injection is `close_connection`,
        // which the generic peer task half-closes (the reader then sees EOF below).
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &state,
            server_id,
            connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            protocol.clone(),
            state.clone(),
            server_id,
            connection_id.as_u32(),
            write_half.clone(),
            status_tx.clone(),
        );

        let result = Self::run_connection(
            read_half,
            &write_half,
            remote_addr,
            connection_id,
            server_id,
            &llm,
            &state,
            &status_tx,
            &protocol,
            &mut session,
        )
        .await;

        // Every exit path — EOF, read error, 1 MiB overflow, injected close — lands here.
        if let Some(task) = session.play_task.take() {
            task.abort();
        }
        state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        state
            .close_connection_on_server(server_id, connection_id)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_connection(
        mut read_half: tokio::io::ReadHalf<TcpStream>,
        write_half: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<TcpStream>>>,
        remote_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm: &OllamaClient,
        state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        protocol: &Arc<RtspProtocol>,
        session: &mut Session,
    ) -> Result<()> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];

        loop {
            // Extract as many complete requests as the buffer holds before reading more.
            while let Some((req, consumed)) = parse_rtsp_request(&buffer) {
                buffer.drain(..consumed);
                Log::new(Some(status_tx)).trace(format!("RTSP {} {}", req.method, req.uri));
                let response = Self::process_request(
                    &req,
                    remote_addr,
                    connection_id,
                    server_id,
                    llm,
                    state,
                    status_tx,
                    session,
                    protocol.as_ref(),
                )
                .await;
                let bytes = response.into_bytes();
                {
                    let mut w = write_half.lock().await;
                    w.write_all(&bytes).await?;
                    w.flush().await?;
                }
                state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(bytes.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;
            }

            let n = read_half.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    Some(n as u64),
                    None,
                    Some(1),
                    None,
                )
                .await;
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.len() > 1_048_576 {
                anyhow::bail!("RTSP request exceeded 1 MiB");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_request(
        req: &RtspRequest,
        remote_addr: SocketAddr,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        llm: &OllamaClient,
        state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
        session: &mut Session,
        protocol: &RtspProtocol,
    ) -> String {
        let base = serde_json::json!({
            "peer_addr": remote_addr.to_string(),
            "connection_id": connection_id.to_string(),
            "uri": req.uri,
            "cseq": req.cseq,
            "method": req.method,
        });

        let event = match req.method.as_str() {
            "OPTIONS" => Event {
                event_type: &RTSP_OPTIONS_EVENT,
                data: base,
            },
            "DESCRIBE" => Event {
                event_type: &RTSP_DESCRIBE_EVENT,
                data: base,
            },
            "SETUP" => {
                let mut d = base;
                d["transport"] =
                    serde_json::json!(req.headers.get("transport").cloned().unwrap_or_default());
                Event {
                    event_type: &RTSP_SETUP_EVENT,
                    data: d,
                }
            }
            "PLAY" => Event {
                event_type: &RTSP_PLAY_EVENT,
                data: base,
            },
            "TEARDOWN" => Event {
                event_type: &RTSP_TEARDOWN_EVENT,
                data: base,
            },
            _ => Event {
                event_type: &RTSP_OTHER_EVENT,
                data: base,
            },
        };

        let action =
            match call_llm(llm, state, server_id, Some(connection_id), &event, protocol).await {
                Ok(result) => {
                    let action = result.raw_actions.into_iter().next();
                    if action.is_none() {
                        // The model was reachable and said nothing. Distinct in the log from a
                        // backend failure and from an explicit refusal; the RTSP framing
                        // defaults below then apply.
                        Log::new(Some(status_tx)).warn(format!(
                            "RTSP {} {} decision=model_no_answer (no action returned)",
                            req.method, req.uri
                        ));
                    }
                    action
                }
                Err(e) => {
                    // Fail closed: answer so the client's request completes deterministically
                    // rather than hanging, and never fabricate a stream. The peer gets a
                    // category only — the error itself goes to the log and the status stream.
                    let failure = crate::utils::WireFailure::classify(&e);
                    let (code, reason, decision, extra) = match failure {
                        // Transient saturation: 503 + Retry-After tells the client to back off
                        // rather than record a permanent fault.
                        crate::utils::WireFailure::Overloaded => (
                            503u16,
                            "Service Unavailable",
                            "fail_closed_llm_overloaded",
                            vec![("Retry-After".to_string(), "5".to_string())],
                        ),
                        crate::utils::WireFailure::Unavailable => (
                            500u16,
                            "Internal Server Error",
                            "fail_closed_llm_error",
                            Vec::new(),
                        ),
                    };
                    Log::new(Some(status_tx)).error(format!(
                        "RTSP {} {} decision={} -> {} {}: {}",
                        req.method, req.uri, decision, code, reason, e
                    ));
                    return build_response(code, reason, &req.cseq, &extra, None, None);
                }
            };

        // An explicit non-2xx from the model is a refusal, not an absence of one. Keep it
        // distinguishable in the log from both branches above (radius' decision= shape).
        if let Some(code) = action
            .as_ref()
            .and_then(|a| a.get("status_code"))
            .and_then(|v| v.as_u64())
        {
            if !(200..300).contains(&code) {
                Log::new(Some(status_tx)).warn(format!(
                    "RTSP {} {} decision=model_reject -> {}",
                    req.method, req.uri, code
                ));
            }
        }

        match req.method.as_str() {
            "OPTIONS" => {
                let methods = action
                    .as_ref()
                    .and_then(|a| a.get("public_methods"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_else(|| "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN".to_string());
                build_response(
                    status_of(&action, 200),
                    "OK",
                    &req.cseq,
                    &[("Public".into(), methods)],
                    None,
                    None,
                )
            }
            "DESCRIBE" => {
                let sdp = action
                    .as_ref()
                    .and_then(|a| a.get("sdp"))
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| default_sdp(&req.uri));
                build_response(
                    status_of(&action, 200),
                    "OK",
                    &req.cseq,
                    &[("Content-Type".into(), "application/sdp".into())],
                    Some(sdp),
                    None,
                )
            }
            "SETUP" => Self::handle_setup(req, remote_addr, &action, status_tx, session).await,
            "PLAY" => {
                Self::handle_play(
                    req,
                    &action,
                    connection_id,
                    server_id,
                    state,
                    status_tx,
                    session,
                )
                .await
            }
            "TEARDOWN" => {
                if let Some(task) = session.play_task.take() {
                    task.abort();
                }
                session.rtp_socket = None;
                session.client_rtp_addr = None;
                let extra = session
                    .id
                    .as_ref()
                    .map(|id| vec![("Session".to_string(), id.clone())])
                    .unwrap_or_default();
                build_response(status_of(&action, 200), "OK", &req.cseq, &extra, None, None)
            }
            _ => build_response(
                status_of(&action, 501),
                "Not Implemented",
                &req.cseq,
                &[],
                None,
                None,
            ),
        }
    }

    /// SETUP: allocate a server-side RTP UDP socket, remember the client's RTP port, and return a
    /// well-formed Transport + Session. The framing is owned by Rust; the model only gates the
    /// status code.
    async fn handle_setup(
        req: &RtspRequest,
        remote_addr: SocketAddr,
        action: &Option<serde_json::Value>,
        status_tx: &mpsc::UnboundedSender<String>,
        session: &mut Session,
    ) -> String {
        let transport = req.headers.get("transport").cloned().unwrap_or_default();
        let client_rtp_port = parse_client_rtp_port(&transport);

        // Bind a UDP socket for outbound RTP on the address family the client reached us on.
        //
        // This used to hardcode 127.0.0.1, which works for a loopback test and for nothing else:
        // a client on another host got a SETUP that succeeded, a Transport header naming a real
        // server_port, a 200 OK to its PLAY — and then every `send_to` failed, because a
        // loopback-bound socket cannot reach an off-host address. The session looked established
        // and no media ever arrived, which is the worst shape a failure can take.
        let bind_addr = match remote_addr.ip() {
            std::net::IpAddr::V4(_) => SocketAddr::from(([0u8, 0, 0, 0], 0)),
            std::net::IpAddr::V6(_) => SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
        };
        let rtp_socket = match UdpSocket::bind(bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                Log::new(Some(status_tx))
                    .error(format!("RTSP SETUP could not bind RTP socket: {}", e));
                return build_response(500, "Internal Server Error", &req.cseq, &[], None, None);
            }
        };
        let server_rtp_port = rtp_socket.local_addr().map(|a| a.port()).unwrap_or(0);

        let session_id = session
            .id
            .clone()
            .unwrap_or_else(|| format!("{:08X}", rand::random::<u32>()));

        if let Some(port) = client_rtp_port {
            session.client_rtp_addr = Some(SocketAddr::new(remote_addr.ip(), port));
        }
        session.rtp_socket = Some(rtp_socket);
        session.server_rtp_port = Some(server_rtp_port);
        session.id = Some(session_id.clone());

        let transport_resp = format!(
            "RTP/AVP;unicast;client_port={}-{};server_port={}-{}",
            client_rtp_port.unwrap_or(0),
            client_rtp_port.map(|p| p + 1).unwrap_or(0),
            server_rtp_port,
            server_rtp_port + 1
        );
        // FileOnly: the SETUP response action's own log_template already reports
        // "-> RTSP {status_code} SETUP" to the TUI at INFO.
        Log::new(Some(status_tx)).debug(format!(
            "RTSP SETUP session={} client_rtp={:?} server_rtp={}",
            session_id, client_rtp_port, server_rtp_port
        ));
        build_response(
            status_of(action, 200),
            "OK",
            &req.cseq,
            &[
                ("Transport".into(), transport_resp),
                ("Session".into(), session_id),
            ],
            None,
            None,
        )
    }

    /// PLAY: start streaming synthesized RTP to the client's RTP port. Content is decided by the
    /// model's play action (tone/dtmf/silence); framing by the shared media engine.
    #[allow(clippy::too_many_arguments)]
    async fn handle_play(
        req: &RtspRequest,
        action: &Option<serde_json::Value>,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
        session: &mut Session,
    ) -> String {
        let (socket, client_addr, session_id) = match (
            session.rtp_socket.clone(),
            session.client_rtp_addr,
            session.id.clone(),
        ) {
            (Some(s), Some(c), Some(id)) => (s, c, id),
            _ => {
                Log::new(Some(status_tx)).warn("RTSP PLAY before a completed SETUP; refusing");
                return build_response(
                    455,
                    "Method Not Valid In This State",
                    &req.cseq,
                    &[],
                    None,
                    None,
                );
            }
        };

        // Media description from the play action (VNC-style: structured, not bytes).
        //
        // A *malformed* description is refused rather than substituted. Both of these used to
        // swallow the parse error and fall back to PCMU/440 Hz, so a model that asked for
        // `content:"dtmf"` and forgot `digits`, or named a codec this engine cannot synthesize,
        // got a confident 200 OK and a tone it never asked for — with the reason discarded. An
        // absent field still takes the documented default; only a value that fails to parse is
        // an error, and RTSP has a status code for it.
        let codec = match action
            .as_ref()
            .and_then(|a| a.get("payload_type"))
            .and_then(|v| v.as_str())
        {
            Some(pt) => match AudioCodec::parse(pt) {
                Ok(c) => c,
                Err(e) => {
                    Log::new(Some(status_tx)).warn(format!(
                        "RTSP PLAY decision=fail_closed_bad_action; refusing to stream: {}",
                        e
                    ));
                    return build_response(400, "Bad Request", &req.cseq, &[], None, None);
                }
            },
            None => AudioCodec::Pcmu,
        };
        let content = match action.as_ref().map(media::parse_audio_content) {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                Log::new(Some(status_tx)).warn(format!(
                    "RTSP PLAY decision=fail_closed_bad_action; refusing to stream: {}",
                    e
                ));
                return build_response(400, "Bad Request", &req.cseq, &[], None, None);
            }
            None => media::AudioContent::Tone { hz: 440.0 },
        };
        let duration_ms = action
            .as_ref()
            .and_then(|a| a.get("duration_ms"))
            .and_then(|v| v.as_u64())
            .unwrap_or(5000);

        let ssrc: u32 = action
            .as_ref()
            .and_then(|a| a.get("ssrc"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or_else(rand::random);

        // Abort any previous stream on this session.
        if let Some(task) = session.play_task.take() {
            task.abort();
        }

        let stx = status_tx.clone();
        let state_clone = state.clone();
        let task = tokio::spawn(async move {
            let payload = match media::synthesize(codec, &content, duration_ms) {
                Ok(p) => p,
                Err(e) => {
                    Log::new(Some(&stx)).warn(format!("RTSP PLAY synthesis: {}", e));
                    return;
                }
            };
            let mut packetizer = RtpPacketizer::new(ssrc, codec.payload_type(), None, None);
            let packets = packetizer.packetize(&payload, media::G711_SAMPLES_PER_FRAME);
            let mut sent = 0u64;
            let mut bytes = 0u64;
            for pkt in &packets {
                if socket.send_to(pkt, client_addr).await.is_err() {
                    break;
                }
                sent += 1;
                bytes += pkt.len() as u64;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            // Count the streamed RTP against THIS connection (the media leaves over a
            // separate UDP socket, but it belongs to this RTSP session).
            state_clone
                .update_connection_stats(
                    server_id,
                    connection_id,
                    None,
                    Some(bytes),
                    None,
                    Some(sent),
                )
                .await;
            // FileOnly: analogous to rtp's send_rtp_audio summary, kept off the TUI to avoid
            // adding a second line alongside the PLAY response's own INFO log_template.
            Log::new(Some(&stx)).debug(format!(
                "RTSP PLAY streamed {} RTP packet(s) to {} ({} bytes)",
                sent, client_addr, bytes
            ));
        });
        session.play_task = Some(task);

        debug!("RTSP PLAY session={} → {}", session_id, client_addr);
        let rtp_info = format!("url={};seq={};rtptime={}", req.uri, 0, 0);
        build_response(
            status_of(action, 200),
            "OK",
            &req.cseq,
            &[
                ("Session".into(), session_id),
                ("RTP-Info".into(), rtp_info),
            ],
            None,
            None,
        )
    }
}

/// Status code override: the model may set `status_code`; otherwise use the method default.
fn status_of(action: &Option<serde_json::Value>, default: u16) -> u16 {
    action
        .as_ref()
        .and_then(|a| a.get("status_code"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(default)
}

/// Build an RTSP/1.0 response with CSeq echoed and an optional SDP/body.
fn build_response(
    status_code: u16,
    default_reason: &str,
    cseq: &str,
    extra_headers: &[(String, String)],
    body: Option<String>,
    reason_override: Option<&str>,
) -> String {
    let reason = reason_override.unwrap_or_else(|| reason_phrase(status_code, default_reason));
    let mut resp = format!("RTSP/1.0 {} {}\r\n", status_code, reason);
    resp.push_str(&format!("CSeq: {}\r\n", cseq));
    resp.push_str("Server: NetGet-RTSP\r\n");
    for (k, v) in extra_headers {
        resp.push_str(&format!("{}: {}\r\n", k, v));
    }
    if let Some(b) = &body {
        resp.push_str(&format!("Content-Length: {}\r\n", b.len()));
        resp.push_str("\r\n");
        resp.push_str(b);
    } else {
        resp.push_str("Content-Length: 0\r\n\r\n");
    }
    resp
}

fn reason_phrase(code: u16, default: &str) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        455 => "Method Not Valid In This State",
        457 => "Invalid Range",
        461 => "Unsupported Transport",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => {
            // `default` is a caller-supplied &str; the reason must stay `&'static` so nothing
            // derived from an error can reach the status line. Fall back to a category phrase
            // rather than "OK", which would make a refusal read as `RTSP/1.0 403 OK`.
            let _ = default;
            match code {
                100..=199 => "Continue",
                200..=299 => "OK",
                300..=399 => "Redirection",
                400..=499 => "Client Error",
                _ => "Server Error",
            }
        }
    }
}

/// Default SDP when the model does not shape one: a single PCMU audio stream.
fn default_sdp(uri: &str) -> String {
    format!(
        "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=NetGet Stream\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n\
         m=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=control:{}\r\n",
        uri
    )
}

/// Extract the first client RTP port from a Transport header (`client_port=A-B`).
fn parse_client_rtp_port(transport: &str) -> Option<u16> {
    for part in transport.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("client_port=") {
            let first = rest.split('-').next()?;
            return first.trim().parse().ok();
        }
    }
    None
}

/// Parse one complete RTSP request from `buf`. Returns the request and how many bytes it
/// consumed, or None if the buffer does not yet hold a full message.
fn parse_rtsp_request(buf: &[u8]) -> Option<(RtspRequest, usize)> {
    // Locate the header terminator on the BYTES, and decode only the header block.
    //
    // This used to run `std::str::from_utf8` over the whole buffer first, which made the parser
    // fail on anything the buffer merely happened to end in the middle of: a multi-byte
    // character split across two TCP segments, or a binary body, invalidated the entire buffer
    // — including a complete request sitting in front of it. The connection then stalled with a
    // fully-formed request unanswered until the 1 MiB overflow closed it.
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header_block = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = header_block.split("\r\n");

    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();
    let _version = parts.next()?; // RTSP/1.0

    let mut headers = HashMap::new();
    for line in lines {
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim().to_ascii_lowercase();
            let value = line[idx + 1..].trim().to_string();
            headers.insert(name, value);
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let total = header_end + 4 + content_length;
    if buf.len() < total {
        return None; // body not fully arrived yet
    }

    let cseq = headers.get("cseq").cloned().unwrap_or_default();
    Some((
        RtspRequest {
            method,
            uri,
            cseq,
            headers,
        },
        total,
    ))
}
