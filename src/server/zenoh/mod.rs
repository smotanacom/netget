//! Zenoh router or peer over the zenoh runtime. Rust declares the subscribers and queryables
//! from the startup parameters, hands each sample and query to the handler and runs its actions
//! on the session; the link list becomes the connection list.
pub mod actions;
pub mod node;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use ::zenoh::sample::Locality;
use ::zenoh::Session;
use anyhow::{Context, Result};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

pub const DEFAULT_MODE: &str = "router";
/// Handler actions that raise further events (a get's result) chain at most this deep.
pub const MAX_CHAIN: usize = 4;
pub const MAX_QUERIES_IN_FLIGHT: usize = 64;
const LINK_POLL: Duration = Duration::from_secs(1);

fn outcome(ctx: &SpawnContext, operation: &str, decision: &str) {
    let summary = format!("Zenoh operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's actions for an event, or Err when it could not answer.
async fn ask(
    ctx: &SpawnContext,
    event: Event,
    operation: &str,
) -> Result<Vec<Json>, anyhow::Error> {
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        None,
        &event,
        &actions::ZenohProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, operation, "fail_closed_llm_error");
            return Err(e);
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, operation, "fail_closed_invalid_reply");
        anyhow::bail!("the handler's answer was not valid");
    }
    let mut out = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => out.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    out.reverse();
    outcome(ctx, operation, "model_answer");
    Ok(out)
}

/// Run a handler's session actions; a get's replies are a new event, followed to MAX_CHAIN.
async fn act(ctx: &SpawnContext, session: &Session, actions: Vec<Json>, depth: usize) {
    for a in actions {
        if !matches!(
            a["type"].as_str(),
            Some("zenoh_put" | "zenoh_delete" | "zenoh_get")
        ) {
            continue;
        }
        match node::perform(session, &a).await {
            Ok(Some(result)) if depth < MAX_CHAIN => {
                if let Ok(next) = ask(
                    ctx,
                    Event::new(&actions::GET_RESULT_EVENT, result),
                    "get_result",
                )
                .await
                {
                    Box::pin(act(ctx, session, next, depth + 1)).await;
                }
            }
            Ok(_) => {}
            Err(e) => Log::new(Some(&ctx.status_tx)).warn(format!("Zenoh {}: {e:#}", a["type"])),
        }
    }
}

fn addr_of(locator: &str) -> Option<SocketAddr> {
    locator.split_once('/').and_then(|(_, a)| a.parse().ok())
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let mode = p
        .map(|p| p.get_optional_string("mode"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_MODE.to_owned());
    anyhow::ensure!(
        matches!(mode.as_str(), "router" | "peer"),
        "mode is router or peer"
    );
    let subscribe = node::keys(
        p.map(|p| p.get_optional_array("subscribe"))
            .transpose()?
            .flatten(),
    )?;
    let queryable = node::keys(
        p.map(|p| p.get_optional_array("queryable"))
            .transpose()?
            .flatten(),
    )?;
    let bind = ctx.legacy_listen_addr();
    let listen = format!("tcp/{bind}");
    let session = ::zenoh::open(node::config(&mode, Some(&listen), None)?)
        .await
        .map_err(|e| anyhow::anyhow!("zenoh: {e}"))?;
    let local = session
        .info()
        .locators()
        .await
        .iter()
        .find_map(|l| addr_of(&l.to_string()))
        .context("the zenoh session reports no TCP locator")?;
    Log::new(Some(&ctx.status_tx)).info(format!("Zenoh {mode} on tcp/{local}"));
    let server_id = ctx.server_id;

    for key in subscribe {
        let sub = session
            .declare_subscriber(key.clone())
            .allowed_origin(Locality::Remote)
            .await
            .map_err(|e| anyhow::anyhow!("subscribe {key}: {e}"))?;
        let (c, s) = (ctx.clone(), session.clone());
        ctx.state
            .spawn_server_task(server_id, async move {
                while let Ok(sample) = sub.recv_async().await {
                    if let Ok(actions) = ask(
                        &c,
                        Event::new(&actions::SAMPLE_EVENT, node::sample_json(&sample)),
                        "sample",
                    )
                    .await
                    {
                        act(&c, &s, actions, 0).await;
                    }
                }
            })
            .await;
    }
    let in_flight = Arc::new(Semaphore::new(MAX_QUERIES_IN_FLIGHT));
    for key in queryable {
        let q = session
            .declare_queryable(key.clone())
            .allowed_origin(Locality::Remote)
            .await
            .map_err(|e| anyhow::anyhow!("queryable {key}: {e}"))?;
        let (c, s, limit) = (ctx.clone(), session.clone(), in_flight.clone());
        ctx.state
            .spawn_server_task(server_id, async move {
                while let Ok(query) = q.recv_async().await {
                    let Ok(permit) = limit.clone().try_acquire_owned() else {
                        let _ = query.reply_err("the server is busy").await;
                        continue;
                    };
                    let (c, s) = (c.clone(), s.clone());
                    let state = c.state.clone();
                    state
                        .spawn_server_task(c.server_id, async move {
                            let _permit = permit;
                            match ask(
                                &c,
                                Event::new(&actions::QUERY_EVENT, node::query_json(&query)),
                                "query",
                            )
                            .await
                            {
                                Ok(actions) => {
                                    if let Err(e) = node::answer(&query, &actions).await {
                                        Log::new(Some(&c.status_tx))
                                            .warn(format!("Zenoh reply: {e:#}"));
                                    }
                                    act(&c, &s, actions, 0).await;
                                }
                                Err(e) => {
                                    let _ = query
                                        .reply_err(crate::utils::WireFailure::classify(&e).text())
                                        .await;
                                }
                            }
                        })
                        .await;
                }
            })
            .await;
    }

    // Links come and go inside the runtime; poll them into the connection list.
    let c = ctx.clone();
    let s = session.clone();
    ctx.state
        .spawn_server_task(server_id, async move {
            let mut known: HashMap<String, ConnectionId> = HashMap::new();
            loop {
                let links = s.info().links().await;
                let mut seen = Vec::new();
                for l in links {
                    let (src, dst) = (l.src().to_string(), l.dst().to_string());
                    let id = format!("{src}|{dst}");
                    seen.push(id.clone());
                    if known.contains_key(&id) {
                        continue;
                    }
                    let conn = ConnectionId::new(c.state.get_next_unified_id().await);
                    let now = Instant::now();
                    c.state
                        .add_connection_to_server(
                            c.server_id,
                            ConnectionState {
                                id: conn,
                                remote_addr: addr_of(&dst)
                                    .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
                                local_addr: addr_of(&src).unwrap_or(local),
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::new(
                                    json!({"zid": l.zid().to_string()}),
                                ),
                            },
                        )
                        .await;
                    known.insert(id, conn);
                }
                let gone: Vec<String> = known
                    .keys()
                    .filter(|k| !seen.contains(k))
                    .cloned()
                    .collect();
                for g in gone {
                    if let Some(conn) = known.remove(&g) {
                        c.state
                            .update_connection_status(c.server_id, conn, ConnectionStatus::Closed)
                            .await;
                    }
                }
                let _ = c.status_tx.send("__UPDATE_UI__".into());
                tokio::time::sleep(LINK_POLL).await;
            }
        })
        .await;
    // The session lives as long as the tasks that hold it; this one keeps it for the server.
    ctx.state
        .spawn_server_task(server_id, async move {
            let _session = session;
            std::future::pending::<()>().await;
        })
        .await;
    Ok(local)
}

/// Every event type, for the client.
pub fn events() -> Vec<&'static EventType> {
    vec![
        &actions::SAMPLE_EVENT,
        &actions::QUERY_EVENT,
        &actions::GET_RESULT_EVENT,
    ]
}
