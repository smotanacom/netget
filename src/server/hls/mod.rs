//! HLS server (HTTP Live Streaming, RFC 8216).
//!
//! Serves an `.m3u8` playlist and its media segments over HTTP/1.1. The model decides the playlist
//! (variants and the segment list — structurally, or as verbatim m3u8 text) and what each segment
//! contains. Real MPEG-TS is binary, so a segment body is either model-supplied text or, for
//! genuine binary, an explicit `encoding: "hex"` field this server decodes for real — never
//! sniffed, never base64-guessed.
//!
//! A self-contained minimal HTTP/1.1 request reader lives here rather than sharing the hyper-based
//! `http` server: HLS needs nothing more than method + path routing, and the `http` server's
//! event/response model is a single `http_request` event, not the two distinct playlist/segment
//! events HLS wants. The framing written here is standard HTTP a real client (curl, ffplay) reads.

pub mod actions;

use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::HlsProtocol;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use crate::{console_error, console_trace};
use actions::{HLS_PLAYLIST_EVENT, HLS_SEGMENT_EVENT};

/// One HTTP response, assembled before it is framed.
///
/// `retry_after` exists so the two failure categories stay distinguishable on the wire: an
/// overloaded backend is transient and a player should back off and re-request, while anything
/// else is a hard 500 it should not hammer.
struct HlsResponse {
    status: u16,
    content_type: String,
    body: Vec<u8>,
    retry_after: bool,
}

impl HlsResponse {
    fn new(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
            retry_after: false,
        }
    }

    /// The peer-visible reply for an internal failure.
    ///
    /// The body is [`WireFailure::prefixed_text`] — a `&'static str` category, never the error.
    /// The error goes to `tracing` and the status stream, which is where an operator looks.
    /// `Overloaded` becomes 503 + `Retry-After` and `Unavailable` becomes 500, so a client backs
    /// off on a transient saturation instead of recording a permanent fault.
    fn failure(failure: WireFailure) -> Self {
        Self {
            status: if failure.is_overloaded() { 503 } else { 500 },
            content_type: "text/plain; charset=utf-8".to_string(),
            body: failure.prefixed_text().as_bytes().to_vec(),
            retry_after: failure.is_overloaded(),
        }
    }
}

/// How long a peer may take to finish sending its request headers.
///
/// HLS clients send one short GET and nothing else, so this is generous for every legitimate
/// case and still bounds a connection that sends a byte at a time.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Longest request path carried into the log, the status stream and the model's prompt.
const MAX_PATH_LEN: usize = 512;

/// Why the peer got what it got, for the log only.
///
/// The three failure cases a reader of `netget.log` must be able to tell apart: the model
/// deliberately answered with an error status, the model answered nothing usable, and the LLM
/// call itself failed. Collapsing them makes an outage look like a policy decision.
const DECISION_MODEL_ANSWER: &str = "model_answer";
const DECISION_MODEL_REJECT: &str = "model_reject";
const DECISION_NO_ANSWER: &str = "fail_closed_no_answer";
const DECISION_BAD_ACTION: &str = "fail_closed_bad_action";
const DECISION_LLM_ERROR: &str = "fail_closed_llm_error";
const DECISION_LLM_OVERLOADED: &str = "fail_closed_llm_overloaded";

/// `model_reject` when the model chose a 4xx/5xx itself, `model_answer` otherwise.
fn decision_for_model_status(status: u16) -> &'static str {
    if status >= 400 {
        DECISION_MODEL_REJECT
    } else {
        DECISION_MODEL_ANSWER
    }
}

pub struct HlsServer;

impl HlsServer {
    /// Spawn the HLS server. Awaits the TCP bind so failure is reported as `Err`, and registers
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
        Log::new(Some(&status_tx)).info(format!("HLS server listening on {}", local_addr));

        let protocol = Arc::new(HlsProtocol::new());
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
                                Log::new(Some(&stx)).debug(format!("HLS connection ended: {}", e));
                            }
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "HLS accept error: {}", e);
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
        protocol: Arc<HlsProtocol>,
    ) -> Result<()> {
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];

        // Read one HTTP request (headers up to the blank line; HLS clients send GETs with no body).
        //
        // Bounded by a deadline as well as by size. Without it a peer that connects and sends
        // one byte — or nothing at all — parks this task and its socket for as long as it cares
        // to keep the connection open, which is the whole of slowloris. 64 KiB caps how much a
        // peer can make us buffer; the deadline caps how long it can make us wait.
        let deadline = tokio::time::Instant::now() + HEADER_READ_TIMEOUT;
        let (method, path) = loop {
            let n = match tokio::time::timeout_at(deadline, read_half.read(&mut chunk)).await {
                Ok(r) => r?,
                Err(_) => anyhow::bail!(
                    "HLS request headers not complete within {}s",
                    HEADER_READ_TIMEOUT.as_secs()
                ),
            };
            if n == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..n]);
            if let Some(req) = parse_request_line(&buffer) {
                break req;
            }
            if buffer.len() > 65536 {
                anyhow::bail!("HLS request headers too large");
            }
        };
        // The path is peer-supplied and can be the better part of 64 KiB. It reaches the log,
        // the status stream and — as event data — the model's prompt, so it is bounded once
        // here rather than at each of those.
        let path = crate::utils::truncate_for_log(&path, MAX_PATH_LEN);
        console_trace!(status_tx, "HLS {} {}", method, path);

        // Refresh connection stats (bytes/packets in) so the dashboard rail shows real traffic
        // and a fresh last_activity rather than ↓0 ↑0. This is a one-shot HTTP request/response
        // connection (`Connection: close`): exactly one read of the request, one write of the
        // response, then the connection returns and closes.
        state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(buffer.len() as u64),
                None,
                Some(1),
                None,
            )
            .await;

        let is_playlist = path.contains(".m3u8");
        let base = serde_json::json!({
            "peer_addr": remote_addr.to_string(),
            "connection_id": connection_id.to_string(),
            "path": path,
            "method": method,
        });
        let event = if is_playlist {
            Event {
                event_type: &HLS_PLAYLIST_EVENT,
                data: base,
            }
        } else {
            Event {
                event_type: &HLS_SEGMENT_EVENT,
                data: base,
            }
        };

        let (reply, decision): (HlsResponse, &str) = match call_llm(
            &llm,
            &state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                let action = result.raw_actions.into_iter().next();
                if is_playlist {
                    render_playlist(&action)
                } else {
                    render_segment(&action)
                }
            }
            Err(e) => {
                // Fail closed. The peer gets a category and a status it can act on; the error
                // itself — backend URL, model name, anyhow chain — stays in the log.
                let failure = WireFailure::classify(&e);
                let decision = if failure.is_overloaded() {
                    DECISION_LLM_OVERLOADED
                } else {
                    DECISION_LLM_ERROR
                };
                error!("HLS {} decision={} error={}", path, decision, e);
                Log::new(Some(&status_tx)).error(format!(
                    "HLS fail-closed for {} (decision={}): {}",
                    path, decision, e
                ));
                (HlsResponse::failure(failure), decision)
            }
        };

        let HlsResponse {
            status,
            content_type,
            body,
            retry_after,
        } = reply;
        let response = build_http_response(status, &content_type, &body, retry_after);
        write_half.write_all(&response).await?;
        write_half.flush().await?;

        state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(response.len() as u64),
                None,
                Some(1),
            )
            .await;
        let _ = status_tx.send(format!(
            "→ HLS {} {} ({} bytes, decision={})",
            status,
            path,
            body.len(),
            decision
        ));
        Ok(())
    }
}

/// Render an `.m3u8` playlist response from the model's action.
///
/// Accepts either a verbatim `playlist` string, or a structured `segments` array
/// (`[{"uri":"seg0.ts","duration":6.0}, …]`) plus optional `target_duration`/`version` which are
/// assembled into a valid media playlist here.
fn render_playlist(action: &Option<serde_json::Value>) -> (HlsResponse, &'static str) {
    let ct = "application/vnd.apple.mpegurl".to_string();
    let Some(action) = action else {
        warn!(
            "HLS playlist decision={} (no action returned)",
            DECISION_NO_ANSWER
        );
        return (
            HlsResponse::failure(WireFailure::Unavailable),
            DECISION_NO_ANSWER,
        );
    };
    let status = action
        .get("status_code")
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as u16;

    if let Some(playlist) = action.get("playlist").and_then(|v| v.as_str()) {
        return (
            HlsResponse::new(status, ct, playlist.as_bytes().to_vec()),
            decision_for_model_status(status),
        );
    }

    if let Some(segments) = action.get("segments").and_then(|v| v.as_array()) {
        let version = action.get("version").and_then(|v| v.as_u64()).unwrap_or(3);
        let target = action
            .get("target_duration")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                segments
                    .iter()
                    .filter_map(|s| s.get("duration").and_then(|d| d.as_f64()))
                    .fold(0.0_f64, f64::max)
                    .ceil() as u64
            })
            .max(1);
        let media_sequence = action
            .get("media_sequence")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut m = String::new();
        m.push_str("#EXTM3U\n");
        m.push_str(&format!("#EXT-X-VERSION:{}\n", version));
        m.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target));
        m.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{}\n", media_sequence));
        for seg in segments {
            let dur = seg
                .get("duration")
                .and_then(|d| d.as_f64())
                .unwrap_or(target as f64);
            let uri = seg
                .get("uri")
                .and_then(|u| u.as_str())
                .unwrap_or("segment.ts");
            m.push_str(&format!("#EXTINF:{:.3},\n{}\n", dur, uri));
        }
        let ended = action
            .get("ended")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if ended {
            m.push_str("#EXT-X-ENDLIST\n");
        }
        return (
            HlsResponse::new(status, ct, m.into_bytes()),
            decision_for_model_status(status),
        );
    }

    // No usable field: fail closed rather than fabricate a playlist. The peer gets a category;
    // what the action actually lacked is a log line for the operator, not for a stranger.
    warn!(
        "HLS playlist decision={} (action lacked both 'playlist' and 'segments')",
        DECISION_NO_ANSWER
    );
    (
        HlsResponse::failure(WireFailure::Unavailable),
        DECISION_NO_ANSWER,
    )
}

/// Render a media segment response. Body is either model text (`content`, utf8) or explicit
/// hex-encoded binary (`encoding: "hex"`, `data`) which is decoded here.
fn render_segment(action: &Option<serde_json::Value>) -> (HlsResponse, &'static str) {
    let Some(action) = action else {
        warn!(
            "HLS segment decision={} (no action returned)",
            DECISION_NO_ANSWER
        );
        return (
            HlsResponse::failure(WireFailure::Unavailable),
            DECISION_NO_ANSWER,
        );
    };
    let status = action
        .get("status_code")
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as u16;
    let content_type = action
        .get("content_type")
        .and_then(|v| v.as_str())
        .unwrap_or("video/mp2t")
        .to_string();

    // Explicit binary path: decode hex for real (the send_tcp_data lesson).
    if let Some(data) = action.get("data").and_then(|v| v.as_str()) {
        let encoding = action
            .get("encoding")
            .and_then(|v| v.as_str())
            .unwrap_or("utf8");
        match encoding {
            "hex" => match hex::decode(data.trim()) {
                Ok(bytes) => {
                    return (
                        HlsResponse::new(status, content_type, bytes),
                        decision_for_model_status(status),
                    )
                }
                Err(e) => {
                    // The decoder's message names offsets in the model's own string; the peer
                    // gets the category, the operator gets the detail.
                    warn!(
                        "HLS segment decision={} (hex decode failed): {}",
                        DECISION_BAD_ACTION, e
                    );
                    return (
                        HlsResponse::failure(WireFailure::Unavailable),
                        DECISION_BAD_ACTION,
                    );
                }
            },
            "utf8" => {
                return (
                    HlsResponse::new(status, content_type, data.as_bytes().to_vec()),
                    decision_for_model_status(status),
                )
            }
            other => {
                warn!(
                    "HLS segment decision={} (unknown encoding {:?}; expected \"utf8\" or \"hex\")",
                    DECISION_BAD_ACTION, other
                );
                return (
                    HlsResponse::failure(WireFailure::Unavailable),
                    DECISION_BAD_ACTION,
                );
            }
        }
    }

    if let Some(content) = action.get("content").and_then(|v| v.as_str()) {
        return (
            HlsResponse::new(status, content_type, content.as_bytes().to_vec()),
            decision_for_model_status(status),
        );
    }

    // An action carrying neither `data` nor `content` is the model answering nothing usable.
    // Serving it as 200 with an empty body would be fail-open: a player accepts a zero-length
    // segment as a valid one and the stream silently plays nothing.
    warn!(
        "HLS segment decision={} (action carried neither 'data' nor 'content')",
        DECISION_NO_ANSWER
    );
    (
        HlsResponse::failure(WireFailure::Unavailable),
        DECISION_NO_ANSWER,
    )
}

/// Build an HTTP/1.1 response. `Connection: close` keeps this a clean one-request-per-connection
/// server, which HLS clients (a new GET per playlist/segment) handle fine.
fn build_http_response(status: u16, content_type: &str, body: &[u8], retry_after: bool) -> Vec<u8> {
    let reason = reason_phrase(status);
    // Only a 503 from the overload path carries Retry-After: it is the signal that tells a
    // player to back off and re-request rather than treat the stream as dead.
    let retry_header = if retry_after {
        "Retry-After: 5\r\n"
    } else {
        ""
    };
    let mut resp = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Cache-Control: no-cache\r\nConnection: close\r\n\r\n",
        status,
        reason,
        content_type,
        body.len(),
        retry_header
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}

/// Reason phrase for a status code.
///
/// The fallback is class-correct rather than a blanket `"OK"`: a model picking 403 used to be
/// framed as `HTTP/1.1 403 OK`, which is a contradiction on the wire.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        416 => "Range Not Satisfiable",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        100..=199 => "Informational",
        200..=299 => "Success",
        300..=399 => "Redirection",
        400..=499 => "Client Error",
        500..=599 => "Server Error",
        _ => "Unknown",
    }
}

/// Parse the method and path from a partial HTTP request. Returns None until the request line and
/// header terminator (`\r\n\r\n`) are present.
fn parse_request_line(buf: &[u8]) -> Option<(String, String)> {
    // Terminator located on the bytes, and only the request line decoded. Running
    // `from_utf8` over the whole buffer rejected a request whose headers were valid because
    // something later in the buffer was not — including a read that merely happened to stop in
    // the middle of a multi-byte character — and the loop then waited for bytes that would
    // never make it valid. The request line itself must still be UTF-8; a path that is not is
    // a request this server cannot route.
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let first = head.lines().next()?;
    let mut parts = first.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    Some((method, path))
}
