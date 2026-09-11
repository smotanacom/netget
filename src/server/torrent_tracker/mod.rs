//! BitTorrent Tracker server implementation
//!
//! HTTP-based tracker for coordinating BitTorrent peers. Handles announce and scrape requests.

pub mod actions;

use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::utils::WireFailure;
use actions::TorrentTrackerProtocol;

/// How long a connected peer may take to send its request line before it is hung up on.
///
/// An announce is one small GET, and every real client sends it in the first segment. There
/// was no bound at all, so a peer that connected and said nothing held a task and a
/// connection-map entry indefinitely — which is a slow-loris for free.
const REQUEST_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// BitTorrent Tracker server
pub struct TorrentTrackerServer;

impl TorrentTrackerServer {
    /// Spawn BitTorrent Tracker server with LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "BitTorrent Tracker server (action-based) listening on {}",
            local_addr
        ));

        let protocol = Arc::new(TorrentTrackerProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        Log::new(Some(&status_clone)).debug(format!(
                            "BitTorrent Tracker accepted connection from {}",
                            peer_addr
                        ));

                        // Add connection to ServerInstance
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
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
                        };
                        state_clone
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_clone.send("__UPDATE_UI__".to_string());

                        tokio::spawn(async move {
                            if let Err(e) = Self::handle_connection(
                                stream,
                                peer_addr,
                                local_addr,
                                connection_id,
                                llm_clone,
                                state_clone,
                                status_clone,
                                server_id,
                                protocol_clone,
                            )
                            .await
                            {
                                error!("BitTorrent Tracker connection error: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("BitTorrent Tracker accept error: {}", e));
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    async fn handle_connection(
        stream: tokio::net::TcpStream,
        peer_addr: SocketAddr,
        _local_addr: SocketAddr,
        connection_id: ConnectionId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: Arc<TorrentTrackerProtocol>,
    ) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut read_half, mut write_half) = tokio::io::split(stream);
        let mut buffer = vec![0u8; 8192];

        // Read HTTP request, bounded.
        //
        // There was no timeout here at all, so a peer that connected and then said nothing
        // held this task — and its entry in the connection map — until it chose to hang up.
        // Opening connections and never writing is the whole of a slow-loris, and it costs
        // the other end nothing. An announce is one small GET that a real client sends in
        // its first segment.
        let n = match tokio::time::timeout(REQUEST_READ_TIMEOUT, read_half.read(&mut buffer)).await
        {
            Ok(result) => result?,
            Err(_) => {
                Log::new(Some(&status_tx)).warn(format!(
                    "BitTorrent Tracker {} sent no request within {}s, closing \
                     decision=fail_closed_read_timeout",
                    peer_addr,
                    REQUEST_READ_TIMEOUT.as_secs()
                ));
                let body = b"HTTP/1.1 408 Request Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = write_half.write_all(body).await;
                return Ok(());
            }
        };
        if n == 0 {
            Log::new(Some(&status_tx)).debug("BitTorrent Tracker connection closed by peer");
            return Ok(());
        }

        let request_data = buffer[..n].to_vec();

        // Refresh connection stats (bytes/packets in) so the rail shows real
        // traffic and last_activity rather than ↓0 ↑0. This is a one-shot HTTP
        // request/response connection, so there is exactly one read and one write.
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(n as u64),
                None,
                Some(1),
                None,
            )
            .await;

        // DEBUG: Log summary
        Log::new(Some(&status_tx)).debug(format!(
            "BitTorrent Tracker received {} bytes from {}",
            n, peer_addr
        ));

        // TRACE: Log full request
        if let Ok(request_str) = std::str::from_utf8(&request_data) {
            Log::new(Some(&status_tx))
                .trace(format!("BitTorrent Tracker request: {}", request_str));
        }

        // Parse HTTP request. A malformed request used to propagate out of this function
        // with `?`, so the connection closed without writing anything and the client saw
        // an empty reply rather than an error.
        let request_str = String::from_utf8_lossy(&request_data);
        let (request_type, request_params) = match Self::parse_http_request(&request_str) {
            Ok(parsed) => parsed,
            Err(e) => {
                Log::new(Some(&status_tx)).warn(format!("BitTorrent Tracker bad request: {}", e));
                let body =
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                write_half.write_all(body).await?;
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(body.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;
                return Ok(());
            }
        };

        Log::new(Some(&status_tx))
            .debug(format!("BitTorrent Tracker request type: {}", request_type));

        // Create event for LLM
        let event_type = match request_type.as_str() {
            "announce" => &actions::TRACKER_ANNOUNCE_REQUEST_EVENT,
            "scrape" => &actions::TRACKER_SCRAPE_REQUEST_EVENT,
            // Anything that is neither /announce nor /scrape still reaches the announce
            // handler, because a tracker has no other reply shape to offer. The event data
            // carries `request_type` and `path` so a handler can tell the difference and
            // answer with send_error_response instead.
            other => {
                tracing::warn!(
                    "BitTorrent Tracker: unrecognised path type '{}', routing to \
                     tracker_announce_request",
                    other
                );
                &actions::TRACKER_ANNOUNCE_REQUEST_EVENT
            }
        };
        let event = Event::new(event_type, serde_json::json!(request_params));

        Log::new(Some(&status_tx)).debug(format!(
            "BitTorrent Tracker calling LLM for {} request",
            request_type
        ));

        // Call LLM
        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                // Display messages from LLM
                for message in &execution_result.messages {
                    Log::new(Some(&status_tx)).info(format!("{}", message));
                }

                Log::new(Some(&status_tx)).debug(format!(
                    "BitTorrent Tracker got {} protocol results",
                    execution_result.protocol_results.len()
                ));

                // An explicit refusal by the model is a `failure reason` it asked for
                // itself; keep it distinguishable in the log from our own fail-closed
                // reply, which says nothing about what the model thought.
                let model_rejected = execution_result
                    .raw_actions
                    .iter()
                    .any(|a| a.get("type").and_then(|t| t.as_str()) == Some("send_error_response"));
                let mut sent_any = false;

                // Send responses
                for protocol_result in execution_result.protocol_results {
                    if let Some(output_data) = protocol_result.get_all_output().first() {
                        sent_any = true;
                        write_half.write_all(output_data).await?;
                        app_state
                            .update_connection_stats(
                                server_id,
                                connection_id,
                                None,
                                Some(output_data.len() as u64),
                                None,
                                Some(1),
                            )
                            .await;

                        Log::new(Some(&status_tx)).debug(format!(
                            "BitTorrent Tracker sent {} bytes to {}",
                            output_data.len(),
                            peer_addr
                        ));

                        // TRACE: Log full response
                        if let Ok(response_str) = std::str::from_utf8(output_data) {
                            Log::new(Some(&status_tx))
                                .trace(format!("BitTorrent Tracker response: {}", response_str));
                        }
                    }
                }

                if sent_any {
                    Log::new(Some(&status_tx)).debug(format!(
                        "BitTorrent Tracker {} from {} decision={}",
                        request_type,
                        peer_addr,
                        if model_rejected {
                            "model_reject"
                        } else {
                            "model_answer"
                        }
                    ));
                } else {
                    // The model produced nothing that reaches the wire. An announce is a
                    // request/response exchange, so silence leaves the client waiting out
                    // its own timeout and then marking this tracker dead — answer with a
                    // category instead.
                    Log::new(Some(&status_tx)).warn(format!(
                        "BitTorrent Tracker {} from {} decision=fail_closed_no_action",
                        request_type, peer_addr
                    ));
                    Self::write_failure_response(
                        &mut write_half,
                        WireFailure::Unavailable,
                        connection_id,
                        server_id,
                        &app_state,
                    )
                    .await?;
                }
            }
            Err(e) => {
                // The error itself goes to the log and the status stream only. The client
                // gets a status code and a fixed category string — never the backend URL,
                // the model name or an `anyhow` context chain.
                let failure = WireFailure::classify(&e);
                Log::new(Some(&status_tx)).error(format!(
                    "BitTorrent Tracker {} from {} decision=fail_closed_llm_error \
                     category={:?}: {}",
                    request_type, peer_addr, failure, e
                ));
                Self::write_failure_response(
                    &mut write_half,
                    failure,
                    connection_id,
                    server_id,
                    &app_state,
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Answer a request netget cannot serve, with a category and nothing more.
    ///
    /// The two categories map onto different status codes so a client backs off rather than
    /// recording a permanent fault: **503 + `Retry-After`** for a saturated backend, **500**
    /// for everything else. This used to be a bare
    /// `HTTP/1.1 500 Internal Server Error\r\n\r\n` for both — no `Content-Length`, no
    /// `Connection: close`, and no way for a client to tell "come back later" from "this
    /// tracker is broken".
    ///
    /// The body is a bencoded `failure reason`, because that is the one refusal BEP 3
    /// defines and the only thing a BitTorrent client will actually display. Its text is
    /// [`WireFailure::text`], a `&'static str`.
    async fn write_failure_response(
        write_half: &mut tokio::io::WriteHalf<tokio::net::TcpStream>,
        failure: WireFailure,
        connection_id: ConnectionId,
        server_id: crate::state::ServerId,
        app_state: &Arc<AppState>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let mut body_dict = std::collections::HashMap::new();
        body_dict.insert(
            b"failure reason".to_vec(),
            serde_bencode::value::Value::Bytes(failure.text().as_bytes().to_vec()),
        );
        let body = serde_bencode::to_bytes(&serde_bencode::value::Value::Dict(body_dict))
            .unwrap_or_default();

        let head = if failure.is_overloaded() {
            format!(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 30\r\nContent-Type: \
                 text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
        } else {
            format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: \
                 text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
        };

        let mut response = head.into_bytes();
        response.extend_from_slice(&body);
        write_half.write_all(&response).await?;
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(response.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    fn parse_http_request(request: &str) -> Result<(String, HashMap<String, serde_json::Value>)> {
        // Parse HTTP request line
        let first_line = request
            .lines()
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty request"))?;

        let parts: Vec<&str> = first_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(anyhow::anyhow!("Invalid HTTP request"));
        }

        let path = parts[1];

        // Determine request type (announce or scrape)
        let request_type = if path.starts_with("/announce") {
            "announce"
        } else if path.starts_with("/scrape") {
            "scrape"
        } else {
            "unknown"
        };

        // Parse query parameters.
        //
        // `request_type`, `path` and `compact` are always inserted: they are declared as
        // event parameters, and a static handler referencing `{{event.compact}}` is a hard
        // error if the field is missing, which it would be for any client that omits the
        // query parameter.
        let mut params = HashMap::new();
        params.insert(
            "request_type".to_string(),
            serde_json::json!(request_type.to_string()),
        );
        params.insert("path".to_string(), serde_json::json!(path.to_string()));
        params.insert("compact".to_string(), serde_json::json!(0u64));
        if let Some(query_start) = path.find('?') {
            let query = &path[query_start + 1..];
            for param in query.split('&') {
                if let Some(eq_pos) = param.find('=') {
                    // Decode key (always UTF-8)
                    let key = urlencoding::decode(&param[..eq_pos])
                        .unwrap_or_else(|_| std::borrow::Cow::Borrowed(&param[..eq_pos]))
                        .into_owned();

                    // For binary fields (info_hash, peer_id), manually percent-decode without UTF-8 validation
                    if key == "info_hash" || key == "peer_id" {
                        let value_str = &param[eq_pos + 1..];
                        let bytes = percent_decode_bytes(value_str);
                        params.insert(key, serde_json::json!(hex::encode(&bytes)));
                    } else {
                        // For other fields, decode as UTF-8
                        let value = urlencoding::decode(&param[eq_pos + 1..])
                            .unwrap_or_else(|_| std::borrow::Cow::Borrowed(&param[eq_pos + 1..]))
                            .into_owned();

                        if key == "port"
                            || key == "uploaded"
                            || key == "downloaded"
                            || key == "left"
                            || key == "numwant"
                            || key == "compact"
                        {
                            // Numeric fields
                            if let Ok(num) = value.parse::<u64>() {
                                params.insert(key, serde_json::json!(num));
                            } else {
                                params.insert(key, serde_json::json!(value));
                            }
                        } else {
                            params.insert(key, serde_json::json!(value));
                        }
                    }
                }
            }
        }

        Ok((request_type.to_string(), params))
    }
}

/// Percent-decode bytes without UTF-8 validation (for binary fields like info_hash)
fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chars = s.chars();

    while let Some(ch) = chars.next() {
        if ch == '%' {
            // Read next two hex digits
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte);
                    continue;
                }
            }
            // If parsing failed, just add the '%' and hex chars as-is
            result.push(b'%');
            result.extend(hex.bytes());
        } else {
            // Non-escaped character
            result.extend(ch.to_string().bytes());
        }
    }

    result
}
