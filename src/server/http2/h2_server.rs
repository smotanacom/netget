//! HTTP/2 server implementation using h2 crate directly for full server push support

use bytes::Bytes;
use h2::server::{self, SendResponse};
use http::{Request, Response, StatusCode};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::http_common::handler::{RequestData, RequestFilter};
use crate::server::Http2Protocol;
use crate::state::app_state::AppState;

use super::actions::HTTP2_REQUEST_EVENT;
use super::push::PendingPush;

/// HTTP/2 server with full server push support
pub struct H2Server;

impl H2Server {
    /// Spawn HTTP/2 server using h2 crate directly
    pub async fn spawn_with_push_support(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        tls_config: Option<Arc<rustls::ServerConfig>>,
    ) -> anyhow::Result<SocketAddr> {
        // Same reuse semantics as the HTTP/1.1 listener, so a restart on the
        // same port does not fail with EADDRINUSE while the old socket lingers.
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        let protocol_name = if tls_config.is_some() {
            "HTTP/2 (TLS, h2 with push)"
        } else {
            "HTTP/2 (h2c with push)"
        };
        info!("{} server listening on {}", protocol_name, local_addr);
        let _ = status_tx.send(format!(
            "[INFO] {} server listening on {}",
            protocol_name, local_addr
        ));

        let protocol = Arc::new(Http2Protocol::new());

        // Build the per-server request filter once, up front. This is the code
        // path `Http2Protocol::spawn()` actually uses, so without this the
        // `request_filter` startup param would be silently ignored for HTTP/2.
        let filter = Arc::new(RequestFilter::from_startup_params(
            app_state
                .get_server(server_id)
                .await
                .and_then(|s| s.startup_params)
                .as_ref(),
        ));
        // Fail-open parsing: surface any dropped rule loudly, since the effect
        // is that more requests reach the LLM, not fewer.
        for warning in filter.warnings() {
            let _ = status_tx.send(format!("[ERROR] HTTP/2 request_filter: {}", warning));
        }

        // Create TLS acceptor if TLS is enabled
        let tls_acceptor = tls_config.map(|config| tokio_rustls::TlsAcceptor::from(config));

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((tcp_stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = tcp_stream.local_addr().unwrap_or(local_addr);
                        info!(
                            "Accepted {} connection {} from {}",
                            protocol_name, connection_id, remote_addr
                        );

                        // Add connection to ServerInstance
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
                            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                                "recent_requests": []
                            })),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();
                        let tls_acceptor_clone = tls_acceptor.clone();
                        let filter_clone = filter.clone();

                        // Spawn task to handle this connection
                        tokio::spawn(async move {
                            // Perform TLS handshake if TLS is enabled
                            let result: Result<(), Box<dyn std::error::Error + Send + Sync>> =
                                if let Some(acceptor) = tls_acceptor_clone {
                                    match acceptor.accept(tcp_stream).await {
                                        Ok(tls_stream) => {
                                            debug!(
                                                "{} TLS handshake complete with {}",
                                                protocol_name, remote_addr
                                            );
                                            let _ = status_tx_clone.send(format!(
                                                "[DEBUG] {} TLS handshake complete with {}",
                                                protocol_name, remote_addr
                                            ));
                                            handle_h2_connection(
                                                tls_stream,
                                                connection_id,
                                                server_id,
                                                llm_client_clone,
                                                app_state_clone.clone(),
                                                status_tx_clone.clone(),
                                                protocol_clone,
                                                filter_clone,
                                            )
                                            .await
                                        }
                                        Err(e) => {
                                            error!("{} TLS handshake failed: {}", protocol_name, e);
                                            let _ = status_tx_clone.send(format!(
                                                "[ERROR] {} TLS handshake failed: {}",
                                                protocol_name, e
                                            ));
                                            Err(Box::new(e))
                                        }
                                    }
                                } else {
                                    // No TLS, use plain TCP (h2c)
                                    handle_h2_connection(
                                        tcp_stream,
                                        connection_id,
                                        server_id,
                                        llm_client_clone,
                                        app_state_clone.clone(),
                                        status_tx_clone.clone(),
                                        protocol_clone,
                                        filter_clone,
                                    )
                                    .await
                                };

                            if let Err(e) = result {
                                error!("{} connection error: {}", protocol_name, e);
                            }

                            // Mark connection as closed
                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_tx_clone.send(format!(
                                "✗ {} connection {connection_id} closed",
                                protocol_name
                            ));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept HTTP/2 connection: {}", e);
                        break;
                    }
                }
            }
        });

        // Without this, stop_server cannot cancel the accept loop and the socket
        // stays bound after the server is removed.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Handle a single HTTP/2 connection with full server push support
#[allow(clippy::too_many_arguments)]
async fn handle_h2_connection<T>(
    tcp_stream: T,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<Http2Protocol>,
    filter: Arc<RequestFilter>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Create h2 connection
    let mut h2_conn = server::handshake(tcp_stream).await?;
    debug!("HTTP/2 handshake complete for connection {}", connection_id);

    // Handle incoming requests
    while let Some(result) = h2_conn.accept().await {
        let (request, send_response) = result?;

        let llm_clone = llm_client.clone();
        let app_state_clone = app_state.clone();
        let status_clone = status_tx.clone();
        let protocol_clone = protocol.clone();
        let filter_clone = filter.clone();

        // Spawn task for each request (stream)
        tokio::spawn(async move {
            if let Err(e) = handle_h2_request(
                request,
                send_response,
                connection_id,
                server_id,
                llm_clone,
                app_state_clone,
                status_clone,
                protocol_clone,
                filter_clone,
            )
            .await
            {
                error!("Error handling HTTP/2 request: {}", e);
            }
        });
    }

    Ok(())
}

/// Build an `http::Response<()>` from model- or config-supplied parts without
/// panicking and without failing the whole request: an out-of-range status
/// becomes 500 and headers hyper rejects (e.g. containing CR/LF) are dropped.
fn build_h2_response_head(
    status: u16,
    headers: impl IntoIterator<Item = (String, String)>,
    context: &str,
) -> Response<()> {
    let status_code = StatusCode::from_u16(status).unwrap_or_else(|_| {
        error!(
            "{}: invalid HTTP status {} (must be 100-599), sending 500 instead",
            context, status
        );
        StatusCode::INTERNAL_SERVER_ERROR
    });

    let mut builder = Response::builder().status(status_code);
    for (name, value) in headers {
        match (
            http::header::HeaderName::from_bytes(name.as_bytes()),
            http::header::HeaderValue::from_str(&value),
        ) {
            (Ok(n), Ok(v)) => builder = builder.header(n, v),
            _ => warn!("{}: dropping invalid response header {:?}", context, name),
        }
    }

    builder.body(()).unwrap_or_else(|e| {
        error!(
            "{}: failed to build response ({}), sending bare 500",
            context, e
        );
        let mut fallback = Response::new(());
        *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
        fallback
    })
}

/// Handle a single HTTP/2 request with server push support
#[allow(clippy::too_many_arguments)]
pub async fn handle_h2_request(
    request: Request<h2::RecvStream>,
    mut send_response: SendResponse<Bytes>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<Http2Protocol>,
    filter: Arc<RequestFilter>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Extract request metadata
    let method = request.method().to_string();
    // For HTTP/2, only use the path+query portion (not scheme/host)
    let uri = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(request.uri().path())
        .to_string();
    let version = format!("{:?}", request.version());

    // Extract headers
    let mut headers = HashMap::new();
    for (name, value) in request.headers() {
        if let Ok(value_str) = value.to_str() {
            headers.insert(name.to_string(), value_str.to_string());
        }
    }

    // Read request body from h2::RecvStream, bounded by the same cap HTTP/1.1 uses.
    // `release_capacity` re-opens the flow-control window after every chunk, so without
    // a total limit a peer can stream an unbounded amount into this Vec — and the body
    // ends up in an LLM prompt, where anything past a few kilobytes is cost with no
    // benefit. Over the cap the stream is answered 413 and never reaches the model.
    let mut body_stream = request.into_body();
    let mut body_bytes = Vec::new();
    let mut body_too_large = false;

    loop {
        match body_stream.data().await {
            Some(Ok(chunk)) => {
                if body_bytes.len() + chunk.len()
                    > crate::server::http_common::MAX_REQUEST_BODY_BYTES
                {
                    body_too_large = true;
                    break;
                }
                body_bytes.extend_from_slice(&chunk);
                // Release flow control capacity for this chunk
                let _ = body_stream.flow_control().release_capacity(chunk.len());
            }
            Some(Err(e)) => {
                warn!("Error reading request body: {}", e);
                let _ = status_tx.send(format!("[WARN] Error reading body: {}", e));
                break;
            }
            None => {
                // End of stream
                break;
            }
        }
    }

    if body_too_large {
        warn!(
            "HTTP/2 {} {} decision=refused_body_too_large: over {} bytes",
            method,
            uri,
            crate::server::http_common::MAX_REQUEST_BODY_BYTES
        );
        let _ = status_tx.send(format!("→ HTTP/2 {} {} → 413", method, uri));
        let body = format!(
            "Payload Too Large: request bodies are limited to {} bytes\n",
            crate::server::http_common::MAX_REQUEST_BODY_BYTES
        );
        let body_len = body.len() as u64;
        let response = build_h2_response_head(
            413,
            [("content-type".to_string(), "text/plain".to_string())],
            "HTTP/2 payload too large",
        );
        let mut stream = send_response.send_response(response, false)?;
        stream.send_data(Bytes::from(body), true)?;
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                None,
                Some(body_len),
                None,
                Some(1),
            )
            .await;
        return Ok(());
    }

    // Record the inbound message against the *connection* (streams are not in the
    // connection map). This has to happen before the filter check so a filtered
    // request still refreshes last_activity: cleanup_old_connections evicts any
    // connection idle for 10s, and an HTTP/2 connection normally outlives that.
    // A "packet" is one HTTP/2 message; bytes are body only, since h2 has already
    // consumed the HEADERS frame by this point.
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            Some(body_bytes.len() as u64),
            None,
            Some(1),
            None,
        )
        .await;

    // Log request
    debug!(
        "HTTP/2 request: {} {} {} ({} bytes) from {:?}",
        method,
        uri,
        version,
        body_bytes.len(),
        connection_id
    );
    let _ = status_tx.send(format!(
        "[DEBUG] HTTP/2 request: {} {} {} ({} bytes)",
        method,
        uri,
        version,
        body_bytes.len()
    ));

    // Apply the per-server request filter before spending an LLM call: only
    // allowlisted requests reach the model, everything else gets the configured
    // auto-response (default 404). Same semantics as HTTP/1.1.
    let path = uri.split('?').next().unwrap_or(&uri).to_string();
    if !filter.is_pass_through() {
        let request_data = RequestData {
            method: method.clone(),
            uri: uri.clone(),
            version: version.clone(),
            headers: headers.clone(),
            body_bytes: Bytes::from(body_bytes.clone()),
        };
        if !filter.allows(&request_data, &path) {
            let (status, hdrs, body) = filter.rejection_parts();
            let _ = status_tx.send(format!(
                "↩ HTTP/2 filtered {} {} → {} (no LLM call)",
                method, path, status
            ));
            let body_len = body.len() as u64;
            let response = build_h2_response_head(status, hdrs, "HTTP/2 filtered_response");
            let mut stream = send_response.send_response(response, false)?;
            stream.send_data(Bytes::from(body), true)?;
            app_state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    None,
                    Some(body_len),
                    None,
                    Some(1),
                )
                .await;
            return Ok(());
        }
    }

    // Create event for LLM.
    //
    // Request bodies are attacker-controlled and need not be UTF-8. Action/event design
    // rules forbid handing the model raw bytes or base64, so the body is always presented
    // as (lossily) decoded text — but a non-UTF-8 body is flagged explicitly rather than
    // silently mangled into U+FFFD, so the model knows the text it sees is not the real
    // payload. Same contract as HTTP/1.1.
    let body_is_binary = std::str::from_utf8(&body_bytes).is_err();
    let body_text = String::from_utf8_lossy(&body_bytes);
    let mut event_data = serde_json::json!({
        "method": method,
        "uri": uri,
        "version": version,
        "headers": headers,
        "body": if body_text.is_empty() { "" } else { body_text.as_ref() },
        "body_bytes": body_bytes.len()
    });
    if body_is_binary {
        event_data["body_is_binary"] = serde_json::Value::Bool(true);
    }
    let event = Event::new(&HTTP2_REQUEST_EVENT, event_data);

    // Call LLM to generate response
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
            debug!("LLM HTTP/2 response received");

            // Display messages
            for msg in execution_result.messages {
                let _ = status_tx.send(msg);
            }

            // Extract response and pushes from protocol results
            let mut status_code = 200;
            let mut response_headers = HashMap::new();
            let mut response_body = String::new();
            let mut pushes = Vec::new();
            // Did any action actually yield a response? Without this the 200 above is emitted
            // whenever the model answered with nothing, with output that is not JSON, or with
            // JSON carrying no status/headers/body — an empty 200 that a client reads as a
            // successful, empty resource. A server push alone does not count: a PUSH_PROMISE
            // is an extra resource offered alongside an answer, not the answer itself.
            let mut produced_response = false;

            for protocol_result in execution_result.protocol_results {
                match protocol_result {
                    ActionResult::Output(output_data) => {
                        // Parse JSON output
                        if let Ok(json_value) =
                            serde_json::from_slice::<serde_json::Value>(&output_data)
                        {
                            // Check if this is a push directive
                            if json_value
                                .get("_push_directive")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                // This is a push request
                                let push = PendingPush {
                                    path: json_value
                                        .get("path")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("/")
                                        .to_string(),
                                    method: json_value
                                        .get("method")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("GET")
                                        .to_string(),
                                    status: json_value
                                        .get("status")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(200)
                                        as u16,
                                    headers: json_value
                                        .get("headers")
                                        .and_then(|v| v.as_object())
                                        .map(|obj| {
                                            obj.iter()
                                                .filter_map(|(k, v)| {
                                                    v.as_str().map(|s| (k.clone(), s.to_string()))
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default(),
                                    body: json_value
                                        .get("body")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .as_bytes()
                                        .to_vec(),
                                };
                                pushes.push(push);
                            } else {
                                // This is the main HTTP response
                                if let Some(status) =
                                    json_value.get("status").and_then(|v| v.as_u64())
                                {
                                    status_code = status as u16;
                                    produced_response = true;
                                }
                                if let Some(headers_obj) =
                                    json_value.get("headers").and_then(|v| v.as_object())
                                {
                                    for (k, v) in headers_obj {
                                        if let Some(v_str) = v.as_str() {
                                            response_headers.insert(k.clone(), v_str.to_string());
                                            produced_response = true;
                                        }
                                    }
                                }
                                if let Some(body) = json_value.get("body").and_then(|v| v.as_str())
                                {
                                    response_body = body.to_string();
                                    produced_response = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Execute server pushes BEFORE sending main response
            for push in pushes {
                debug!("Executing server push for {}", push.path);
                let push_body_len = push.body.len();

                // Create push promise request
                let mut push_request = http::Request::builder()
                    .method(push.method.as_str())
                    .uri(&push.path);

                for (name, value) in &push.headers {
                    push_request = push_request.header(name, value);
                }

                if let Ok(push_req) = push_request.body(()) {
                    // Send push promise
                    match send_response.push_request(push_req) {
                        Ok(mut push_stream) => {
                            // Send push response
                            let mut push_response = http::Response::builder().status(push.status);

                            for (name, value) in &push.headers {
                                push_response = push_response.header(name, value);
                            }

                            if let Ok(push_resp) = push_response.body(()) {
                                match push_stream.send_response(push_resp, false) {
                                    Ok(mut stream) => {
                                        if let Err(e) =
                                            stream.send_data(Bytes::from(push.body), true)
                                        {
                                            warn!(
                                                "Failed to send push body for {}: {}",
                                                push.path, e
                                            );
                                        } else {
                                            debug!("Successfully pushed {}", push.path);
                                            let _ = status_tx.send(format!(
                                                "⬆ Pushed {} ({} bytes)",
                                                push.path, push_body_len
                                            ));
                                            // A push is an extra message on the wire.
                                            app_state
                                                .update_connection_stats(
                                                    server_id,
                                                    connection_id,
                                                    None,
                                                    Some(push_body_len as u64),
                                                    None,
                                                    Some(1),
                                                )
                                                .await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to send push response for {}: {}",
                                            push.path, e
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Client rejected push for {}: {}", push.path, e);
                        }
                    }
                }
            }

            // Nothing usable came back.
            //
            // If the operator configured a `default_response`, that is a deliberate
            // answer for exactly this case and it is used — the parameter is declared by
            // `request_handling_startup_parameters()`, which HTTP/2 advertises, and until
            // now HTTP/2 read it nowhere, so setting it changed nothing here.
            //
            // With no `default_response`, refuse rather than emitting the pre-set 200,
            // which a client cannot tell from a real, empty answer.
            if !produced_response {
                if let Some((status, hdrs, body)) = filter.default_response_parts() {
                    warn!(
                        "HTTP/2 {} {} decision=default_response: actions ran but none \
                         yielded a response; answering the configured default ({})",
                        method, uri, status
                    );
                    let _ = status_tx.send(format!(
                        "→ HTTP/2 {} {} → {} (configured default_response)",
                        method, uri, status
                    ));
                    let body_len = body.len() as u64;
                    let response = build_h2_response_head(status, hdrs, "HTTP/2 default_response");
                    let mut stream = send_response.send_response(response, false)?;
                    stream.send_data(Bytes::from(body), true)?;
                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            None,
                            Some(body_len),
                            None,
                            Some(1),
                        )
                        .await;
                    return Ok(());
                }

                warn!(
                    "HTTP/2 {} {} decision=fail_closed_no_action: actions ran but none \
                     yielded a response",
                    method, uri
                );
                let _ = status_tx.send(format!(
                    "✗ HTTP/2 {} {} → 500 (no response action produced)",
                    method, uri
                ));
                let body = crate::utils::WireFailure::Unavailable.prefixed_text();
                let response = Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(())?;
                let mut stream = send_response.send_response(response, false)?;
                stream.send_data(Bytes::from(body), true)?;
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

            // Send main response
            let _ = status_tx.send(format!(
                "→ HTTP/2 {} {} → {} ({} bytes)",
                method,
                uri,
                status_code,
                response_body.len()
            ));

            // Status and headers come from model output; never let a bogus
            // value abort the request without a response.
            let body_len = response_body.len() as u64;
            let response = build_h2_response_head(status_code, response_headers, "HTTP/2");
            let mut stream = send_response.send_response(response, false)?;
            stream.send_data(Bytes::from(response_body), true)?;
            app_state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    None,
                    Some(body_len),
                    None,
                    Some(1),
                )
                .await;
        }
        Err(e) => {
            // Same split HTTP/1.1 makes in `http_common::build_error_response`, which root
            // CLAUDE.md names as the shape to copy: an *overloaded* backend is transient
            // and gets 503 + `Retry-After` so the client backs off, anything else gets 500
            // and is treated as a permanent fault. HTTP/2 answered a flat 500 for both,
            // so a client could not tell "come back in a second" from "this is broken".
            //
            // Only the category reaches the peer; `e` is logged and never rendered onto
            // the wire.
            let failure = crate::utils::WireFailure::classify(&e);
            let (status, decision) = match failure {
                crate::utils::WireFailure::Overloaded => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "fail_closed_llm_overloaded",
                ),
                crate::utils::WireFailure::Unavailable => {
                    (StatusCode::INTERNAL_SERVER_ERROR, "fail_closed_llm_error")
                }
            };
            error!(
                "HTTP/2 {} {} decision={} status={}: {}",
                method,
                uri,
                decision,
                status.as_u16(),
                e
            );
            let _ = status_tx.send(format!(
                "✗ LLM error for {} {} → {}: {}",
                method,
                uri,
                status.as_u16(),
                e
            ));

            let error_body = failure.prefixed_text();
            let mut builder = Response::builder().status(status);
            if failure.is_overloaded() {
                builder = builder.header(http::header::RETRY_AFTER, "1");
            }
            let response = builder.body(())?;
            let mut stream = send_response.send_response(response, false)?;
            stream.send_data(Bytes::from(error_body), true)?;
            app_state
                .update_connection_stats(
                    server_id,
                    connection_id,
                    None,
                    Some(error_body.len() as u64),
                    None,
                    Some(1),
                )
                .await;
        }
    }

    Ok(())
}
