//! Zenoh client or peer over the zenoh runtime, sharing the server's node code.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::zenoh::{actions as zactions, node, MAX_CHAIN};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use ::zenoh::sample::Locality;
use ::zenoh::Session;
pub use actions::ZenohClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value as Json};
use std::net::SocketAddr;
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_MODE: &str = "client";
const LINK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// An event for the handler, with where its answer goes when it must answer a query.
struct Asked {
    event: Event,
    answer: Option<oneshot::Sender<Vec<Json>>>,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let mode = p
        .map(|p| p.get_optional_string("mode"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_MODE.to_owned());
    anyhow::ensure!(
        matches!(mode.as_str(), "client" | "peer"),
        "mode is client or peer"
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
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await?
        .next()
        .context("the address does not resolve")?;
    let endpoint = format!("tcp/{remote}");
    let session = ::zenoh::open(node::config(&mode, None, Some(&endpoint))?)
        .await
        .map_err(|e| anyhow::anyhow!("zenoh: {e}"))?;
    // A peer opens before its link is up; wait for the link rather than report none.
    let deadline = tokio::time::Instant::now() + LINK_WAIT;
    let links: Vec<Json> = loop {
        let links: Vec<Json> = session
            .info()
            .links()
            .await
            .map(|l| json!({"src": l.src().to_string(), "dst": l.dst().to_string(), "zid": l.zid().to_string()}))
            .collect();
        if !links.is_empty() {
            break links;
        }
        if tokio::time::Instant::now() > deadline {
            let _ = session.close().await;
            anyhow::bail!("no link to {endpoint} within {} s", LINK_WAIT.as_secs());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let local = links[0]["src"]
        .as_str()
        .and_then(|s| s.split_once('/'))
        .and_then(|(_, a)| a.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let zid = session.info().zid().await.to_string();

    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Json, usize)>(64);
    let (ask_tx, mut ask_rx) = mpsc::channel::<(Asked, usize)>(64);
    ask_tx.try_send((
        Asked {
            event: Event::new(
                &actions::CONNECTED_EVENT,
                json!({"zid": zid, "mode": mode, "links": links}),
            ),
            answer: None,
        },
        0,
    ))?;

    // One dispatcher asks the handler, in order; query answers go back to the waiting query.
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = ZenohClientProtocol;
        while let Some((asked, depth)) = ask_rx.recv().await {
            let instruction = events_ctx
                .state
                .get_instruction_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = events_ctx
                .state
                .get_memory_for_client(events_ctx.client_id)
                .await
                .unwrap_or_default();
            let result = call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&asked.event),
                &protocol,
                &events_ctx.status_tx,
            )
            .await;
            let actions = match result {
                Ok(r) => {
                    if let Some(m) = r.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, m)
                            .await;
                    }
                    r.actions
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx))
                        .warn(format!("Zenoh client handler: {e}"));
                    vec![]
                }
            };
            if let Some(reply) = asked.answer {
                let _ = reply.send(actions.clone());
            }
            for a in actions {
                if matches!(
                    a["type"].as_str(),
                    Some("zenoh_put" | "zenoh_delete" | "zenoh_get" | "disconnect")
                ) && internal_tx.send((a, depth)).await.is_err()
                {
                    return;
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    for key in subscribe {
        let sub = session
            .declare_subscriber(key.clone())
            .allowed_origin(Locality::Remote)
            .await
            .map_err(|e| anyhow::anyhow!("subscribe {key}: {e}"))?;
        let tx = ask_tx.clone();
        let task = tokio::spawn(async move {
            while let Ok(sample) = sub.recv_async().await {
                if tx
                    .send((
                        Asked {
                            event: Event::new(&zactions::SAMPLE_EVENT, node::sample_json(&sample)),
                            answer: None,
                        },
                        0,
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        ctx.state.register_client_task(ctx.client_id, task).await;
    }
    for key in queryable {
        let q = session
            .declare_queryable(key.clone())
            .allowed_origin(Locality::Remote)
            .await
            .map_err(|e| anyhow::anyhow!("queryable {key}: {e}"))?;
        let tx = ask_tx.clone();
        let status = ctx.status_tx.clone();
        let task = tokio::spawn(async move {
            while let Ok(query) = q.recv_async().await {
                let (back, answer) = oneshot::channel();
                if tx
                    .send((
                        Asked {
                            event: Event::new(&zactions::QUERY_EVENT, node::query_json(&query)),
                            answer: Some(back),
                        },
                        0,
                    ))
                    .await
                    .is_err()
                {
                    return;
                }
                match answer.await {
                    Ok(actions) => {
                        if let Err(e) = node::answer(&query, &actions).await {
                            Log::new(Some(&status)).warn(format!("Zenoh reply: {e:#}"));
                        }
                    }
                    Err(_) => {
                        let _ = query.reply_err("the client could not answer").await;
                    }
                }
            }
        });
        ctx.state.register_client_task(ctx.client_id, task).await;
    }

    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &session, external, internal_rx, &ask_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("Zenoh client ended: {e:#}"));
        }
        let _ = session.close().await;
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
            .await;
        session_ctx
            .state
            .remove_client_handle(session_ctx.client_id)
            .await;
        let _ = session_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(local)
}

async fn run(
    ctx: &ConnectContext,
    session: &Session,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Json, usize)>,
    ask: &mpsc::Sender<(Asked, usize)>,
) -> Result<()> {
    loop {
        let (action, depth, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), 0, Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some((a, d)) => (a, d, None), None => return Ok(()) },
        };
        let outcome = match ZenohClientProtocol.execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_)
                if matches!(
                    action["type"].as_str(),
                    Some("zenoh_reply" | "zenoh_reply_error")
                ) =>
            {
                Ok(ClientSendOutcome::Rejected {
                    error: "a reply only answers a zenoh_query".into(),
                })
            }
            Ok(_) => match node::perform(session, &action).await {
                Ok(Some(result)) => {
                    if depth < MAX_CHAIN {
                        ask.send((
                            Asked {
                                event: Event::new(&zactions::GET_RESULT_EVENT, result),
                                answer: None,
                            },
                            depth + 1,
                        ))
                        .await
                        .ok();
                    }
                    Ok(ClientSendOutcome::Sent { bytes_sent: 0 })
                }
                Ok(None) => Ok(ClientSendOutcome::Sent {
                    bytes_sent: node::payload_from(&action).map(|p| p.len()).unwrap_or(0),
                }),
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: format!("{e:#}"),
                }),
            },
        };
        if let Some(c) = command {
            let logged = outcome
                .as_ref()
                .map(|o| serde_json::to_value(o).unwrap_or(Json::Null))
                .unwrap_or_else(|e: &anyhow::Error| json!({"error": e.to_string()}));
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Zenoh",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![logged],
                )
                .await;
            crate::client::command_support::reply(c, outcome);
        }
    }
}
