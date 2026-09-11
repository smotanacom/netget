//! RSS (Really Simple Syndication) Feed Server
//!
//! Serves RSS 2.0 XML feeds over HTTP with LLM-controlled content.
//! The LLM generates feed content dynamically for each request.

pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::rss::actions::{RssProtocol, RSS_FEED_REQUESTED_EVENT};
use crate::state::app_state::AppState;
use crate::state::server::ServerId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::WireFailure;
use anyhow::Result;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, warn};

use crate::logging::emit::Log;

/// RSS server - generates feeds dynamically via LLM
pub struct RssServer;

impl RssServer {
    /// Spawn RSS server with LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        // SO_REUSEADDR, like every other TCP server here: without it a restart on the same
        // port fails with EADDRINUSE for the length of TIME_WAIT.
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("RSS server listening on {}", local_addr));

        let llm_client = Arc::new(llm_client);
        let protocol = Arc::new(RssProtocol::new());

        // Spawn server loop
        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        debug!("RSS connection from {}", peer_addr);

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);

                        // Without this the connection never appears in the dashboard rail and
                        // its byte counters stay at zero for the life of the server.
                        let now = std::time::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ConnectionState {
                                    id: connection_id,
                                    remote_addr: peer_addr,
                                    local_addr: local_addr_conn,
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

                        let llm = Arc::clone(&llm_client);
                        let state = Arc::clone(&app_state);
                        let status = status_tx.clone();
                        let proto = Arc::clone(&protocol);

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);

                            let conn_state = Arc::clone(&state);
                            let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                                let llm = Arc::clone(&llm);
                                let state = Arc::clone(&state);
                                let status = status.clone();
                                let proto = Arc::clone(&proto);

                                async move {
                                    Self::handle_request(
                                        req,
                                        llm,
                                        state,
                                        status,
                                        server_id,
                                        connection_id,
                                        proto,
                                    )
                                    .await
                                }
                            });

                            if let Err(e) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("RSS connection error: {}", e);
                            }

                            conn_state
                                .remove_connection_from_server(server_id, connection_id)
                                .await;
                        });
                    }
                    Err(e) => {
                        // Break rather than continue: a persistent accept error (EMFILE, the
                        // socket closed under us) spins this loop at full CPU forever,
                        // logging on every iteration. The same fix `vnc` carries.
                        error!("RSS accept failed, stopping loop: {}", e);
                        Log::new(Some(&status_tx))
                            .error(format!("RSS accept failed, stopping loop: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Handle HTTP request for RSS feed - calls LLM to generate content
    #[allow(clippy::too_many_arguments)]
    async fn handle_request(
        req: Request<hyper::body::Incoming>,
        llm_client: Arc<OllamaClient>,
        app_state: Arc<AppState>,
        status_tx: UnboundedSender<String>,
        server_id: ServerId,
        connection_id: ConnectionId,
        protocol: Arc<RssProtocol>,
    ) -> Result<Response<http_body_util::Full<Bytes>>, hyper::Error> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let path = uri.path().to_string();
        let headers_map = req.headers().clone();

        // FileOnly: the rss_feed_requested event's own log_template already reports the
        // request to the TUI at INFO.
        Log::new(Some(&status_tx)).debug(format!("RSS request: {} {}", method, path));

        // Approximate wire size of the request line and headers. hyper does not expose the
        // exact framing; what the rail's `down` counter needs is a number that moves with
        // traffic, not a byte-perfect one.
        let bytes_in = method.as_str().len()
            + uri.to_string().len()
            + headers_map
                .iter()
                .map(|(k, v)| k.as_str().len() + v.len() + 4)
                .sum::<usize>();
        app_state
            .update_connection_stats(
                server_id,
                connection_id,
                Some(bytes_in as u64),
                None,
                Some(1),
                None,
            )
            .await;

        // Only support GET requests
        if method != hyper::Method::GET {
            return Ok(Self::counted(
                &app_state,
                server_id,
                connection_id,
                StatusCode::METHOD_NOT_ALLOWED,
                None,
                Bytes::from("Method Not Allowed"),
            )
            .await);
        }

        // Extract headers
        let mut headers = std::collections::HashMap::new();
        for (key, value) in headers_map.iter() {
            if let Ok(value_str) = value.to_str() {
                headers.insert(key.as_str().to_lowercase(), value_str.to_string());
            }
        }

        // Create event for LLM
        let event = Event::new(
            &RSS_FEED_REQUESTED_EVENT,
            serde_json::json!({
                "path": path,
                "headers": headers,
            }),
        );

        Log::new(Some(&status_tx)).debug(format!("RSS calling LLM for feed: {}", path));

        // Call LLM to generate RSS feed
        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            None,
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                // Log messages
                let log = Log::new(Some(&status_tx));
                for message in &execution_result.messages {
                    log.info(message);
                }

                // An action the executor refused never reaches `protocol_results` at all, so
                // "no feed" and "the feed was rejected" look identical from there. That is the
                // same collapse the 404 below exists to avoid, one layer up: without this, a
                // `generate_rss_feed` missing its required title was answered 404 — the
                // model's own way of saying the path has no feed — and the reason lived only
                // in the log. `failures` is where the executor puts it.
                let mut build_error: Option<String> = execution_result
                    .failures
                    .first()
                    .map(|f| format!("{}: {}", f.action, f.error));

                // Process protocol results
                for protocol_result in execution_result.protocol_results {
                    if let crate::llm::actions::protocol_trait::ActionResult::Custom {
                        name,
                        data,
                    } = protocol_result
                    {
                        if name == "generate_rss_feed" {
                            // Extract RSS feed data from LLM response
                            match Self::build_rss_from_llm_data(data) {
                                Ok(xml) => {
                                    let xml_bytes = xml.into_bytes();
                                    // FileOnly: the generate_rss_feed action's own
                                    // log_template already reports "-> RSS feed: {title}"
                                    // to the TUI at INFO.
                                    Log::new(Some(&status_tx)).debug(format!(
                                        "RSS {} decision=model_feed path={} ({} bytes)",
                                        connection_id,
                                        path,
                                        xml_bytes.len()
                                    ));

                                    return Ok(Self::counted(
                                        &app_state,
                                        server_id,
                                        connection_id,
                                        StatusCode::OK,
                                        Some("application/rss+xml; charset=utf-8"),
                                        Bytes::from(xml_bytes),
                                    )
                                    .await);
                                }
                                Err(e) => {
                                    // Keep looking: a later action may still carry a usable
                                    // feed. If none does, this is why, and it is netget
                                    // refusing the model's data rather than the model
                                    // deciding this path has no feed.
                                    build_error = Some(e.to_string());
                                }
                            }
                        }
                    }
                }

                if let Some(reason) = build_error {
                    // The model answered and the answer was unusable. That is netget
                    // refusing, not "this path has no feed", so it must not be a 404 - a
                    // reader told 404 stops polling. The reason goes to the log; the peer
                    // gets a category.
                    warn!(
                        "RSS {} decision=fail_closed_unusable_feed: {}",
                        connection_id, reason
                    );
                    Log::new(Some(&status_tx)).warn(format!(
                        "RSS {} decision=fail_closed_unusable_feed: {}",
                        connection_id, reason
                    ));
                    return Ok(Self::counted(
                        &app_state,
                        server_id,
                        connection_id,
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Some("text/plain; charset=utf-8"),
                        Bytes::from(WireFailure::Unavailable.prefixed_text()),
                    )
                    .await);
                }

                // No generate_rss_feed at all. This is the model's own decision that the
                // path carries no feed - the only way it has to say so - and 404 is the
                // right answer. It is deliberately distinct from the unusable-feed path
                // above and the backend-failure path below; the `decision=` tag is what
                // tells the three apart in the log, because the status code cannot.
                debug!("RSS {} decision=model_no_feed path={}", connection_id, path);
                Log::new(Some(&status_tx)).debug(format!(
                    "RSS {} decision=model_no_feed path={}",
                    connection_id, path
                ));
                Ok(Self::counted(
                    &app_state,
                    server_id,
                    connection_id,
                    StatusCode::NOT_FOUND,
                    None,
                    Bytes::from("Feed Not Found"),
                )
                .await)
            }
            Err(e) => {
                // The peer gets a category, the log gets the error. A saturated backend is
                // transient and is answered 503 + Retry-After so a reader backs off rather
                // than recording a permanent fault; anything else is 500.
                let failure = WireFailure::classify(&e);
                warn!(
                    "RSS {} decision=fail_closed_llm_error ({:?}): {}",
                    connection_id, failure, e
                );
                Log::new(Some(&status_tx)).warn(format!(
                    "RSS {} decision=fail_closed_llm_error: {}",
                    connection_id, e
                ));

                let (status, retry_after) = if failure.is_overloaded() {
                    (StatusCode::SERVICE_UNAVAILABLE, Some("5"))
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, None)
                };
                let body = Bytes::from(failure.prefixed_text());
                let sent = body.len() as u64;
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(sent),
                        None,
                        Some(1),
                    )
                    .await;
                let mut builder = Response::builder()
                    .status(status)
                    .header("Content-Type", "text/plain; charset=utf-8");
                if let Some(seconds) = retry_after {
                    builder = builder.header("Retry-After", seconds);
                }
                Ok(builder
                    .body(http_body_util::Full::new(body))
                    // Only fails on an invalid status or header, both literals here.
                    .unwrap_or_else(|_| {
                        Response::new(http_body_util::Full::new(Bytes::from_static(b"")))
                    }))
            }
        }
    }

    /// Build a response and record its bytes against the connection.
    ///
    /// Every exit from `handle_request` goes through here (or accounts for itself) so the
    /// rail's `up` counter reflects what was actually written.
    async fn counted(
        app_state: &Arc<AppState>,
        server_id: ServerId,
        connection_id: ConnectionId,
        status: StatusCode,
        content_type: Option<&str>,
        body: Bytes,
    ) -> Response<http_body_util::Full<Bytes>> {
        let sent = body.len() as u64;
        app_state
            .update_connection_stats(server_id, connection_id, None, Some(sent), None, Some(1))
            .await;

        let mut builder = Response::builder().status(status);
        if let Some(ct) = content_type {
            builder = builder.header("Content-Type", ct);
        }
        builder
            .body(http_body_util::Full::new(body))
            // Only fails on an invalid status or header value, and both are ours.
            .unwrap_or_else(|_| Response::new(http_body_util::Full::new(Bytes::from_static(b""))))
    }

    /// Build RSS XML from LLM-generated data.
    ///
    /// Every string that reaches the document goes through [`xml_safe`] first. The `rss`
    /// crate escapes text elements (`BytesText::new` escapes on write) and emits item
    /// descriptions as CDATA split on `]]>` (`BytesCData::escaped`), so model content cannot
    /// forge feed structure — `tests/server/rss/injection_test.rs` measures both. What it
    /// does *not* do is filter the characters XML 1.0 forbids outright (NUL and most of C0),
    /// which no escaping can represent: one of those in a title produces a well-formed-looking
    /// document that every strict parser rejects. That is what `xml_safe` is for.
    fn build_rss_from_llm_data(data: serde_json::Value) -> Result<String> {
        use rss::{CategoryBuilder, ChannelBuilder, ItemBuilder};

        // The three channel fields the action declares `required: true`. They used to be
        // read with `unwrap_or("Untitled Feed")` / `unwrap_or("http://localhost")` /
        // `unwrap_or("No description")`, so an answer that named none of them produced a
        // plausible-looking feed instead of a failure, and the model was never told. A
        // required field whose default asserts a result is the shape this codebase has been
        // bitten by; refuse instead, and let the repair loop see the reason.
        actions::validate_feed_data(&data)?;

        let title = xml_safe(data["title"].as_str().unwrap_or_default());
        let link = xml_safe(data["link"].as_str().unwrap_or_default());
        let description = xml_safe(data["description"].as_str().unwrap_or_default());

        let mut channel_builder = ChannelBuilder::default();
        channel_builder
            .title(title)
            .link(link)
            .description(description);

        // Add optional channel fields
        if let Some(language) = data["language"].as_str() {
            channel_builder.language(Some(xml_safe(language)));
        }
        // Handle ttl as either string or number
        if let Some(ttl_str) = data["ttl"].as_str() {
            channel_builder.ttl(Some(xml_safe(ttl_str)));
        } else if let Some(ttl_num) = data["ttl"].as_u64() {
            channel_builder.ttl(Some(ttl_num.to_string()));
        } else if let Some(ttl_num) = data["ttl"].as_i64() {
            channel_builder.ttl(Some(ttl_num.to_string()));
        }
        if let Some(last_build_date) = data["last_build_date"].as_str() {
            channel_builder.last_build_date(Some(xml_safe(last_build_date)));
        }

        // Extract items
        let mut items = Vec::new();
        if let Some(items_array) = data["items"].as_array() {
            for item_data in items_array {
                let mut item_builder = ItemBuilder::default();

                if let Some(title) = item_data["title"].as_str() {
                    item_builder.title(Some(xml_safe(title)));
                }
                if let Some(link) = item_data["link"].as_str() {
                    item_builder.link(Some(xml_safe(link)));
                }
                if let Some(description) = item_data["description"].as_str() {
                    item_builder.description(Some(xml_safe(description)));
                }
                if let Some(author) = item_data["author"].as_str() {
                    item_builder.author(Some(xml_safe(author)));
                }
                if let Some(pub_date) = item_data["pub_date"].as_str() {
                    item_builder.pub_date(Some(xml_safe(pub_date)));
                }
                if let Some(guid) = item_data["guid"].as_str() {
                    item_builder.guid(Some(
                        rss::GuidBuilder::default().value(xml_safe(guid)).build(),
                    ));
                }

                // Add categories
                if let Some(categories_array) = item_data["categories"].as_array() {
                    let categories: Vec<rss::Category> = categories_array
                        .iter()
                        .filter_map(|cat| {
                            if let Some(cat_str) = cat.as_str() {
                                Some(CategoryBuilder::default().name(xml_safe(cat_str)).build())
                            } else if let Some(cat_obj) = cat.as_object() {
                                let name = xml_safe(cat_obj.get("name")?.as_str()?);
                                let mut builder = CategoryBuilder::default();
                                builder.name(name);
                                if let Some(domain) = cat_obj.get("domain").and_then(|v| v.as_str())
                                {
                                    // A category domain is an XML *attribute*. quick-xml
                                    // escapes attribute values, so a quote cannot close it.
                                    builder.domain(Some(xml_safe(domain)));
                                }
                                Some(builder.build())
                            } else {
                                None
                            }
                        })
                        .collect();
                    item_builder.categories(categories);
                }

                items.push(item_builder.build());
            }
        }

        channel_builder.items(items);
        let channel = channel_builder.build();

        Ok(channel.to_string())
    }
}

/// Drop the characters XML 1.0 §2.2 forbids in a document.
///
/// Escaping does not help here: there is no entity for `&#x0;`, and a raw NUL or C0 control in
/// a title makes the whole feed unparseable to every conforming reader — the server would be
/// answering 200 with a document `feed-rs`, `libxml2` and every browser reject. The legal
/// control characters (tab, LF, CR) are kept, as are all of C1 and beyond, which XML 1.0
/// permits in content.
///
/// Dropping rather than refusing is deliberate: a stray control byte inside model prose is not
/// grounds to refuse the whole feed, and the alternative — passing it through — is the only
/// option that produces a broken response.
fn xml_safe(text: &str) -> String {
    if text
        .chars()
        .all(|c| !matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}'))
    {
        return text.to_string();
    }
    text.chars()
        .filter(|c| !matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}'))
        .collect()
}
