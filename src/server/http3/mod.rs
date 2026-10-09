//! Real HTTP/3. The registered endpoint task owns every session and request future.
pub mod actions;
pub mod wire;
use crate::llm::{action_helper::call_llm, ActionResult};
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::quic::*;
use actions::{Http3Protocol, HTTP3_REQUEST_EVENT};
use anyhow::{bail, ensure, Result};
use bytes::{Buf, Bytes};
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::json;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use wire::*;
#[derive(Clone)]
struct Settings {
    handshake: Duration,
    exchange: Duration,
    idle: Duration,
    connections: usize,
    streams: usize,
}
impl Settings {
    fn read(p: Option<&crate::protocol::StartupParams>) -> Result<Self> {
        Ok(Self {
            handshake: Duration::from_secs(bounded_parameter(p, "handshake_timeout_secs", 10, 60)?),
            exchange: Duration::from_secs(bounded_parameter(p, "exchange_timeout_secs", 30, 300)?),
            idle: Duration::from_secs(bounded_parameter(p, "idle_timeout_secs", 300, 3600)?),
            connections: bounded_parameter(p, "max_connections", 64, 256)? as usize,
            streams: bounded_parameter(p, "max_streams", 32, 32)? as usize,
        })
    }
}
pub struct Http3Server;
impl Http3Server {
    pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
        let settings = Settings::read(ctx.startup_params.as_ref())?;
        let cert = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_string("cert_path"))
            .transpose()?
            .flatten();
        let key = ctx
            .startup_params
            .as_ref()
            .map(|p| p.get_optional_string("key_path"))
            .transpose()?
            .flatten();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = match (cert, key) {
            (Some(c), Some(k)) => {
                crate::server::tls_cert_manager::load_tls_config_from_files(&c, &k)?
            }
            (None, None) => crate::server::tls_cert_manager::generate_default_tls_config()?,
            _ => bail!("cert_path and key_path must be supplied together"),
        };
        let mut tls = (*tls).clone();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        tls.max_early_data_size = 0;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
        ));
        config.transport_config(transport(settings.idle, settings.streams as u32, 4));
        config.migration(false);
        let endpoint = EndpointOwner(EndpointGuard(quinn::Endpoint::server(
            config,
            ctx.legacy_listen_addr(),
        )?));
        let local = endpoint.local_addr()?;
        Log::new(Some(&ctx.status_tx)).info(format!("HTTP3 listening on {local}"));
        let state = ctx.state.clone();
        let id = ctx.server_id;
        let task = tokio::spawn(async move {
            let mut sessions = FuturesUnordered::new();
            loop {
                tokio::select! {
                    incoming=endpoint.accept()=> { let Some(incoming)=incoming else { break; }; if sessions.len()>=settings.connections { incoming.refuse(); continue; } sessions.push(Self::session(incoming,ctx.clone(),settings.clone(),local)); },
                    _=sessions.next(),if !sessions.is_empty()=>{}
                }
            }
        });
        state.register_server_task(id, task).await;
        Ok(local)
    }
    async fn session(
        incoming: quinn::Incoming,
        ctx: SpawnContext,
        settings: Settings,
        local: SocketAddr,
    ) {
        let connection = match tokio::time::timeout(settings.handshake, incoming).await {
            Ok(Ok(c)) => ConnectionOwner(ConnectionGuard(c)),
            _ => return,
        };
        let mut h3 = match h3::server::builder()
            .send_grease(false)
            .max_field_section_size(MAX_HEADERS as u64)
            .build(h3_quinn::Connection::new(connection.clone()))
            .await
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
        let now = crate::utils::clock::Instant::now();
        ctx.state
            .add_connection_to_server(
                ctx.server_id,
                ConnectionState {
                    id,
                    remote_addr: connection.remote_address(),
                    local_addr: local,
                    bytes_sent: 0,
                    bytes_received: 0,
                    packets_sent: 0,
                    packets_received: 0,
                    last_activity: now,
                    status: ConnectionStatus::Active,
                    status_changed_at: now,
                    protocol_info: ProtocolConnectionInfo::new(json!({"alpn":"h3"})),
                },
            )
            .await;
        let mut requests = FuturesUnordered::new();
        loop {
            tokio::select! {
                resolver=h3.accept()=> { match resolver {
                    Ok(Some(resolver))=> { if requests.len()>=settings.streams { connection.close(0x107u32.into(),b"request capacity"); break; } requests.push(Self::request(resolver,ctx.clone(),id,settings.exchange,connection.remote_address())); },
                    _=>break
                } },
                _=requests.next(),if !requests.is_empty()=>{},
                _=connection.closed()=>break,
            }
        }
        drop(requests);
        ctx.state
            .close_connection_on_server(ctx.server_id, id)
            .await;
        let _ = ctx.status_tx.send("__UPDATE_UI__".into());
    }
    async fn request(
        resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
        ctx: SpawnContext,
        id: ConnectionId,
        deadline: Duration,
        peer: SocketAddr,
    ) {
        let end = tokio::time::Instant::now() + deadline;
        let (request, stream) = match tokio::time::timeout_at(end, resolver.resolve_request()).await
        {
            Ok(Ok(r)) => r,
            _ => return,
        };
        let mut guard = ServerStreamGuard {
            stream,
            done: false,
        };
        let exchange = async {
            check_request_headers(request.headers())?;
            let mut body = Vec::new();
            while let Some(mut chunk) = guard.stream.recv_data().await? {
                ensure!(
                    chunk.remaining() <= MAX_BODY.saturating_sub(body.len()),
                    "HTTP3 request body exceeds 8 MiB"
                );
                let count = chunk.remaining();
                body.extend_from_slice(&chunk.copy_to_bytes(count));
            }
            let trailers = guard.stream.recv_trailers().await?.unwrap_or_default();
            check_headers(&trailers)?;
            validate_length(request.headers(), body.len())?;
            let bytes = body.len();
            let text = String::from_utf8(body)?;
            ctx.state
                .update_connection_stats(ctx.server_id, id, Some(bytes as u64), None, Some(1), None)
                .await;
            let event = Event::new(
                &HTTP3_REQUEST_EVENT,
                json!({
                    "method":request.method().as_str(),
                    "path":request.uri().path_and_query().map(|p|p.as_str()).unwrap_or("/"),
                    "headers":request_header_json(request.headers())?, "body":text,
                    "trailers":header_json(&trailers)?, "stream_id":guard.stream.id().index(), "peer_addr":peer.to_string(),
                }),
            );
            Log::new(Some(&ctx.status_tx)).debug(format!(
                "HTTP3 {} {}",
                request.method(),
                request.uri()
            ));
            Log::new(Some(&ctx.status_tx)).trace(format!("HTTP3 request: {}", event.data));
            let log = Log::new(Some(&ctx.status_tx));
            let response = match call_llm(
                &ctx.llm_client,
                &ctx.state,
                ctx.server_id,
                Some(id),
                &event,
                &Http3Protocol,
            )
            .await
            {
                Ok(result) => {
                    for message in result.messages {
                        log.info(message);
                    }
                    if !result.failures.is_empty() {
                        log.error("HTTP3 decision=fail_closed_action_error; answering 500");
                        json!({"status":500,"body":"Response could not be generated"})
                    } else {
                        let mut response = None;
                        for action in result.protocol_results {
                            match action {
                                ActionResult::Custom { name, data } if name == "http3_response" => {
                                    ensure!(response.is_none(), "Multiple final HTTP3 responses");
                                    response = Some(data);
                                }
                                ActionResult::CloseConnection => {
                                    log.info(
                                        "HTTP3 decision=model_reject; request explicitly cancelled",
                                    );
                                    return Ok(false);
                                }
                                _ => {}
                            }
                        }
                        if let Some(response) = response {
                            let decision = if matches!(response["status"].as_u64(), Some(401 | 403))
                            {
                                "model_reject"
                            } else {
                                "model_answer"
                            };
                            log.info(format!(
                                "HTTP3 decision={decision} status={}",
                                response["status"]
                            ));
                            response
                        } else {
                            log.info("HTTP3 decision=model_silent; answering 500");
                            json!({"status":500,"body":"No response provided"})
                        }
                    }
                }
                Err(error) => {
                    let category = if crate::utils::WireFailure::classify(&error).is_overloaded() {
                        "overloaded"
                    } else {
                        "unavailable"
                    };
                    log.error(format!("HTTP3 decision=fail_closed_llm_error category={category}; answering 503: {error}"));
                    json!({"status":503,"body":"Service unavailable"})
                }
            };
            let mut reply = http::Response::builder()
                .status(response["status"].as_u64().unwrap_or(500) as u16)
                .body(())?;
            *reply.headers_mut() = parse_headers(&response["headers"])?;
            let body = response["body"].as_str().unwrap_or("");
            guard.stream.send_response(reply).await?;
            let body = if request.method() == http::Method::HEAD {
                ""
            } else {
                body
            };
            if !body.is_empty() {
                guard
                    .stream
                    .send_data(Bytes::copy_from_slice(body.as_bytes()))
                    .await?;
            }
            let trailers = parse_headers(&response["trailers"])?;
            if !trailers.is_empty() {
                guard.stream.send_trailers(trailers).await?;
            }
            guard.stream.finish().await?;
            ctx.state
                .update_connection_stats(
                    ctx.server_id,
                    id,
                    None,
                    Some(body.len() as u64),
                    None,
                    Some(1),
                )
                .await;
            Ok::<_, anyhow::Error>(true)
        };
        match tokio::time::timeout_at(end, exchange).await {
            Ok(Ok(true)) => guard.done = true,
            Ok(Ok(false)) => {}
            Ok(Err(error)) => Log::new(Some(&ctx.status_tx)).error(format!(
                "HTTP3 decision=fail_closed_request_error; resetting request: {error}"
            )),
            Err(_) => Log::new(Some(&ctx.status_tx))
                .error("HTTP3 decision=fail_closed_timeout; resetting request"),
        }
    }
}
struct ServerStreamGuard {
    stream: h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
    done: bool,
}
impl Drop for ServerStreamGuard {
    fn drop(&mut self) {
        if !self.done {
            self.stream
                .stop_sending(h3::error::Code::H3_REQUEST_CANCELLED);
            self.stream
                .stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
        }
    }
}
