//! ActivityPub instance (server role). HTTP is served by `http.rs` over `instance.rs`;
//! every activity that arrives at an inbox with a valid signature is queued and answered by
//! the model one at a time, in order.
pub mod actions;
pub mod http;
pub mod instance;
pub mod sig;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use instance::{Inbound, Instance};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// Verified activities waiting for the model; past it an inbox answers 503.
pub const INBOX_QUEUE: usize = 256;

/// The base URL ids are built from: the operator's, or http:// and the bound address.
pub fn base_url(configured: Option<String>, local: SocketAddr) -> String {
    configured.unwrap_or_else(|| {
        let host = if local.ip().is_unspecified() {
            "127.0.0.1".to_string()
        } else {
            match local.ip() {
                std::net::IpAddr::V6(v6) => format!("[{v6}]"),
                ip => ip.to_string(),
            }
        };
        format!("http://{host}:{}", local.port())
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let names: Vec<String> = match params
        .map(|p| p.get_optional_array("actors"))
        .transpose()?
        .flatten()
    {
        Some(list) => list
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .context("actors must be usernames")
            })
            .collect::<Result<_>>()?,
        None => actions::DEFAULT_ACTORS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let configured = params
        .map(|p| p.get_optional_string("base_url"))
        .transpose()?
        .flatten();
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    let base = base_url(configured, local);
    let instance = Arc::new(
        tokio::task::spawn_blocking(move || Instance::new(&base, &names))
            .await
            .context("key generation stopped")??,
    );
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ActivityPub instance {} on {local}, actors: {}",
        instance.base,
        instance
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .map(|n| instance.handle(n))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    let (tx, rx) = mpsc::channel(INBOX_QUEUE);
    let server_id = ctx.server_id;
    let consumer_ctx = ctx.clone();
    let consumer_instance = instance.clone();
    ctx.state
        .spawn_server_task(server_id, consume(consumer_ctx, consumer_instance, rx))
        .await;
    let state = ctx.state.clone();
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) =
                match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "ActivityPub", Some(&ctx.status_tx)).await {
                    Ok(v) => v,
                    Err(_) => break,
                };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
                .add_connection_to_server(
                    server_id,
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
                        protocol_info: ProtocolConnectionInfo::empty(),
                    },
                )
                .await;
            let child = ctx.clone();
            let instance = instance.clone();
            let tx = tx.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let log_ctx = child.clone();
                    let service = service_fn(move |request| {
                        let instance = instance.clone();
                        let tx = tx.clone();
                        let ctx = log_ctx.clone();
                        async move {
                            let line = format!("{} {}", request.method(), request.uri().path());
                            let (reply, outcome) = http::handle(&instance, request, &tx).await;
                            let log = Log::new(Some(&ctx.status_tx));
                            match outcome {
                                http::Outcome::Refused(why) => {
                                    log.warn(format!("ActivityPub {line} refused: {why}"))
                                }
                                http::Outcome::Queued => {
                                    log.info(format!("ActivityPub {line} queued for the model"))
                                }
                                http::Outcome::Served => {
                                    log.debug(format!("ActivityPub {line} {}", reply.status()))
                                }
                            }
                            Ok::<_, Infallible>(reply)
                        }
                    });
                    let _ = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_buf_size(64 * 1024)
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                    child
                        .state
                        .update_connection_status(child.server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(local)
}

/// The local actor an action acts as.
pub fn acting(instance: &Instance, action: &Value, inbox_owner: Option<&str>) -> Result<String> {
    let name = action["as"]
        .as_str()
        .map(str::to_string)
        .or_else(|| inbox_owner.map(str::to_string))
        .or_else(|| instance.first_actor())
        .context("the instance has no actors")?;
    anyhow::ensure!(instance.has_actor(&name), "no local actor {name}");
    Ok(name)
}

/// Carry out a post, like, follow or unfollow; what happened, for the log or the handler.
pub async fn apply_shared(
    instance: &Instance,
    me: &str,
    action: &Value,
    sender: Option<&str>,
) -> Result<Value> {
    Ok(match action["type"].as_str().unwrap_or_default() {
        actions::POST => {
            let to: Vec<String> = action["to"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let (create, results) = instance
                .post(
                    me,
                    action["content"].as_str().unwrap_or_default(),
                    &to,
                    action["public"].as_bool().unwrap_or(true),
                    action["in_reply_to"].as_str(),
                )
                .await?;
            json!({"note_id": create["object"]["id"], "deliveries": results.iter().map(|(inbox, r)| match r {
                Ok(s) => json!({"inbox": inbox, "status": s}),
                Err(e) => json!({"inbox": inbox, "error": e}),
            }).collect::<Vec<_>>()})
        }
        actions::LIKE => {
            let to = action["to"]
                .as_str()
                .or(sender)
                .context("whom to tell: give to")?;
            json!({"status": instance.like(me, action["object"].as_str().unwrap_or_default(), to).await?})
        }
        actions::FOLLOW => {
            let (id, status) = instance
                .follow(me, action["target"].as_str().unwrap_or_default())
                .await?;
            json!({"actor": id, "status": status})
        }
        actions::UNFOLLOW => {
            let (id, status) = instance
                .unfollow(me, action["target"].as_str().unwrap_or_default())
                .await?;
            json!({"actor": id, "status": status})
        }
        other => anyhow::bail!("{other} is not something an actor does on its own"),
    })
}

/// What Rust settles before the model hears of an activity: follower lists and follows.
pub fn bookkeeping(instance: &Instance, inb: &Inbound) {
    let a = &inb.activity;
    let obj = &a["object"];
    let to = inb.to.clone().or_else(|| {
        obj["object"]
            .as_str()
            .or(obj.as_str())
            .and_then(|o| instance.local_name(o))
    });
    match (a["type"].as_str(), obj["type"].as_str()) {
        (Some("Undo"), Some("Follow")) => {
            if let Some(name) = obj["object"].as_str().and_then(|o| instance.local_name(o)) {
                instance.remove_follower(&name, &inb.signer);
            }
        }
        (Some(t @ ("Accept" | "Reject")), _) => {
            if let Some(name) = to {
                instance.follow_answered(&name, &inb.signer, t == "Accept");
            }
        }
        _ => {}
    }
}

/// The event the model is shown for an activity.
pub async fn event_data(instance: &Instance, inb: &Inbound) -> Value {
    let mut data = instance::summary(&inb.activity);
    data["to_actor"] = json!(inb.to);
    data["actor"] = json!(inb.signer);
    if let Ok(r) = instance.remote_actor(&inb.signer, inb.to.as_deref()).await {
        if let (Some(user), Ok(u)) = (r.preferred_username, reqwest::Url::parse(&r.id)) {
            let host = match u.port() {
                Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
                None => u.host_str().unwrap_or_default().to_string(),
            };
            data["actor_handle"] = json!(format!("{user}@{host}"));
        }
    }
    data
}

async fn consume(ctx: SpawnContext, instance: Arc<Instance>, mut rx: mpsc::Receiver<Inbound>) {
    let log = Log::new(Some(&ctx.status_tx));
    while let Some(inb) = rx.recv().await {
        bookkeeping(&instance, &inb);
        let data = event_data(&instance, &inb).await;
        let kind = data["type"].as_str().unwrap_or("?").to_string();
        let event = Event::new(&actions::ACTIVITY_EVENT, data);
        let result = match call_llm(
            &ctx.llm_client,
            &ctx.state,
            ctx.server_id,
            None,
            &event,
            &actions::ActivityPubProtocol,
        )
        .await
        {
            Ok(r) if r.failures.is_empty() => r,
            _ => {
                log.error(format!(
                    "ActivityPub {kind} from {} decision=fail_closed_llm_error (nothing sent)",
                    inb.signer
                ));
                continue;
            }
        };
        let mut todo = Vec::new();
        let mut stack = result.protocol_results;
        while let Some(r) = stack.pop() {
            match r {
                ActionResult::Custom { data, .. } => todo.push(data),
                ActionResult::Multiple(items) => stack.extend(items),
                _ => {}
            }
        }
        todo.reverse();
        log.info(format!(
            "ActivityPub {kind} from {} decision={}",
            inb.signer,
            if todo.is_empty() {
                "model_silent"
            } else {
                "model_answer"
            }
        ));
        for action in todo {
            let outcome: Result<Value> = async {
                let me = acting(&instance, &action, inb.to.as_deref())?;
                match action["type"].as_str().unwrap_or_default() {
                    t @ (actions::ACCEPT | actions::REJECT) => {
                        anyhow::ensure!(kind == "Follow", "{t} answers a Follow, not a {kind}");
                        let me = inb.activity["object"]
                            .as_str()
                            .and_then(|o| instance.local_name(o))
                            .unwrap_or(me);
                        let status = instance
                            .answer_follow(&me, &inb.activity, t == actions::ACCEPT)
                            .await?;
                        Ok(json!({"status": status}))
                    }
                    _ => apply_shared(&instance, &me, &action, Some(&inb.signer)).await,
                }
            }
            .await;
            match outcome {
                Ok(v) => log.info(format!("ActivityPub {}: {v}", action["type"])),
                Err(e) => log.warn(format!("ActivityPub {} failed: {e:#}", action["type"])),
            }
        }
    }
}
