//! WebTransport over HTTP/3 on the vendored wtransport. The registered endpoint task owns every
//! session; each session asks the handler whether to admit it, then runs `session::Session`.
pub mod actions;
pub mod session;
use crate::llm::{action_helper::call_llm, ActionResult};
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::{WebTransportProtocol, SESSION_REQUEST_EVENT};
use anyhow::{bail, Context, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::{json, Value};
use sha2::Digest;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use wtransport::endpoint::{IncomingSession, SessionRequest};
use wtransport::{Endpoint, Identity, ServerConfig};

pub const MAX_SESSIONS: u64 = 64;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// From the first packet to the CONNECT request.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Self-signed certificates stay within browsers' serverCertificateHashes limit.
pub const SELF_SIGNED_DAYS: u32 = 14;

fn bounded(
    p: Option<&crate::protocol::StartupParams>,
    name: &str,
    default: u64,
    max: u64,
) -> Result<u64> {
    let value = p
        .map(|p| p.get_optional_u64(name))
        .transpose()?
        .flatten()
        .unwrap_or(default);
    if value == 0 || value > max {
        bail!("{name} must be between 1 and {max}");
    }
    Ok(value)
}

/// The certificate from cert_path/key_path, or a fresh self-signed one.
pub async fn identity(p: Option<&crate::protocol::StartupParams>) -> Result<Identity> {
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    match (get("cert_path")?, get("key_path")?) {
        (Some(c), Some(k)) => Identity::load_pemfiles(&c, &k)
            .await
            .with_context(|| format!("loading {c} and {k}")),
        (None, None) => Ok(Identity::self_signed_builder()
            .subject_alt_names(["localhost", "127.0.0.1", "::1"])
            .from_now_utc()
            .validity_days(SELF_SIGNED_DAYS)
            .build()?),
        _ => bail!("cert_path and key_path must be supplied together"),
    }
}

/// The SHA-256 of the leaf certificate, as browsers' serverCertificateHashes want it.
pub fn certificate_sha256(identity: &Identity) -> String {
    identity
        .certificate_chain()
        .as_slice()
        .first()
        .map(|c| hex::encode(sha2::Sha256::digest(c.der())))
        .unwrap_or_default()
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let max_sessions = bounded(p, "max_sessions", MAX_SESSIONS, 256)? as usize;
    let idle = Duration::from_secs(bounded(
        p,
        "idle_timeout_secs",
        IDLE_TIMEOUT.as_secs(),
        3600,
    )?);
    let _ = rustls::crypto::ring::default_provider().install_default();
    let identity = identity(p).await?;
    let hash = certificate_sha256(&identity);
    let config = ServerConfig::builder()
        .with_bind_address(ctx.legacy_listen_addr())
        .with_identity(identity)
        .max_idle_timeout(Some(idle))?
        .allow_migration(false)
        .build();
    let endpoint = Endpoint::server(config)?;
    let local = endpoint.local_addr()?;
    ctx.state
        .with_server_mut(ctx.server_id, |s| {
            s.set_protocol_field("certificate_sha256".into(), json!(hash))
        })
        .await;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "WebTransport listening on {local} (UDP); certificate sha-256 {hash}"
    ));
    let state = ctx.state.clone();
    let id = ctx.server_id;
    let task = tokio::spawn(async move {
        let mut sessions = FuturesUnordered::new();
        loop {
            tokio::select! {
                incoming = endpoint.accept() => {
                    if sessions.len() >= max_sessions {
                        incoming.refuse();
                        continue;
                    }
                    sessions.push(session(incoming, ctx.clone(), local));
                }
                _ = sessions.next(), if !sessions.is_empty() => {}
            }
        }
    });
    state.register_server_task(id, task).await;
    Ok(local)
}

/// The handler's actions for one event, as raw JSON.
async fn ask(ctx: &SpawnContext, id: ConnectionId, event: Event) -> Result<Vec<Value>> {
    let result = call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &WebTransportProtocol,
    )
    .await?;
    let log = Log::new(Some(&ctx.status_tx));
    for message in result.messages {
        log.info(message);
    }
    if !result.failures.is_empty() {
        bail!("the handler's actions failed: {:?}", result.failures);
    }
    Ok(result
        .protocol_results
        .into_iter()
        .filter_map(|r| match r {
            ActionResult::Custom { data, .. } => Some(data),
            _ => None,
        })
        .collect())
}

async fn session(incoming: IncomingSession, ctx: SpawnContext, local: SocketAddr) {
    let request = match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await {
        Ok(Ok(r)) => r,
        _ => return,
    };
    let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
    let now = crate::utils::clock::Instant::now();
    let peer = request.remote_address();
    ctx.state
        .add_connection_to_server(
            ctx.server_id,
            ConnectionState {
                id,
                remote_addr: peer,
                local_addr: local,
                bytes_sent: 0,
                bytes_received: 0,
                packets_sent: 0,
                packets_received: 0,
                last_activity: now,
                status: ConnectionStatus::Active,
                status_changed_at: now,
                protocol_info: ProtocolConnectionInfo::new(
                    json!({"path": request.path(), "authority": request.authority()}),
                ),
            },
        )
        .await;
    let reason = admit_and_run(request, &ctx, id, peer).await;
    Log::new(Some(&ctx.status_tx))
        .debug(format!("WebTransport session from {peer} ended: {reason}"));
    ctx.state
        .remove_peer_handle(ctx.server_id, id.as_u32())
        .await;
    ctx.state
        .close_connection_on_server(ctx.server_id, id)
        .await;
    let _ = ctx.status_tx.send("__UPDATE_UI__".into());
}

async fn admit_and_run(
    request: SessionRequest,
    ctx: &SpawnContext,
    id: ConnectionId,
    peer: SocketAddr,
) -> String {
    let log = Log::new(Some(&ctx.status_tx));
    let event = Event::new(
        &SESSION_REQUEST_EVENT,
        json!({
            "path": request.path(),
            "authority": request.authority(),
            "origin": request.origin(),
            "headers": request.headers().iter().map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
            "peer_addr": peer.to_string(),
        }),
    );
    let mut actions = match ask(ctx, id, event).await {
        Ok(actions) => actions,
        Err(e) => {
            log.error(format!(
                "WebTransport decision=fail_closed_llm_error; refusing the session with 429: {e}"
            ));
            request.too_many_requests().await;
            return "admission failed".into();
        }
    };
    let decision = actions.iter().position(|a| {
        matches!(
            a["type"].as_str(),
            Some("webtransport_accept" | "webtransport_reject")
        )
    });
    let Some(at) = decision else {
        log.info("WebTransport decision=model_silent; refusing the session with 404");
        request.not_found().await;
        return "no admission decision".into();
    };
    let decided = actions.remove(at);
    if decided["type"] == "webtransport_reject" {
        let status = decided["status"].as_u64().unwrap_or(403);
        log.info(format!(
            "WebTransport decision=model_reject status={status} path={}",
            request.path()
        ));
        match status {
            404 => request.not_found().await,
            429 => request.too_many_requests().await,
            _ => request.forbidden().await,
        }
        return format!("refused with {status}");
    }
    let extra: Vec<(String, String)> = decided["headers"]
        .as_object()
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    log.info(format!(
        "WebTransport decision=model_answer; accepted {}",
        request.path()
    ));
    let conn = match request.accept_with_headers(extra).await {
        Ok(c) => c,
        Err(e) => return format!("accepting failed: {e}"),
    };
    let commands =
        crate::server::peer_support::register_peer_channel(&ctx.state, ctx.server_id, id.as_u32())
            .await;
    let ask_ctx = ctx.clone();
    let traffic_ctx = ctx.clone();
    let s = session::Session::new(
        conn,
        Arc::new(move |event| {
            let ctx = ask_ctx.clone();
            Box::pin(async move { ask(&ctx, id, event).await })
        }),
        Arc::new(move |rx, tx| {
            let ctx = traffic_ctx.clone();
            Box::pin(async move {
                ctx.state
                    .update_connection_stats(
                        ctx.server_id,
                        id,
                        (rx > 0).then_some(rx),
                        (tx > 0).then_some(tx),
                        (rx > 0).then_some(1),
                        (tx > 0).then_some(1),
                    )
                    .await;
            })
        }),
        ctx.status_tx.clone(),
        "WebTransport",
    );
    s.run(actions, None, commands).await
}
