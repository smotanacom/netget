//! ActivityPub client: one actor, served (document, key, inbox) on a local port by the
//! server's instance code, driven by the handler. Verified inbox activities and the outcome
//! of every action are events.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::activitypub::{self as ap, instance::Instance};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ActivityPubClientProtocol;
use anyhow::{Context, Result};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A handler chain (answer → event → answer …) stops after this many follow-ups.
pub const MAX_FOLLOWUP_DEPTH: usize = 8;
/// Connections the actor's endpoint serves at once.
pub const MAX_ENDPOINT_CONNECTIONS: usize = 64;

pub async fn connect(ctx: ConnectContext) -> Result<std::net::SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let username = params
        .map(|p| p.get_optional_string("username"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_USERNAME.into());
    let listen = params
        .map(|p| p.get_optional_string("listen"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| actions::DEFAULT_LISTEN.into());
    let configured = params
        .map(|p| p.get_optional_string("base_url"))
        .transpose()?
        .flatten();
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("cannot serve the actor on {listen}"))?;
    let local = listener.local_addr()?;
    let base = ap::base_url(configured, local);
    let names = vec![username.clone()];
    let instance = Arc::new(
        tokio::task::spawn_blocking(move || Instance::new(&base, &names))
            .await
            .context("key generation stopped")??,
    );
    let actor_id = instance.actor_url(&username);
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ActivityPub client: actor {} ({actor_id}) served on {local}",
        instance.handle(&username)
    ));

    let (inbound_tx, mut inbound_rx) = mpsc::channel(ap::INBOX_QUEUE);
    let endpoint_instance = instance.clone();
    let endpoint_log = ctx.status_tx.clone();
    let endpoint_state = ctx.state.clone();
    let client_id = ctx.client_id;
    let endpoint = tokio::spawn(async move {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_ENDPOINT_CONNECTIONS));
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                continue;
            };
            let instance = endpoint_instance.clone();
            let tx = inbound_tx.clone();
            let log = endpoint_log.clone();
            let conn = async move {
                let _permit = permit;
                let service = service_fn(move |request| {
                    let instance = instance.clone();
                    let tx = tx.clone();
                    let log = log.clone();
                    async move {
                        let line = format!("{} {}", request.method(), request.uri().path());
                        let (reply, outcome) = ap::http::handle(&instance, request, &tx).await;
                        if let ap::http::Outcome::Refused(why) = outcome {
                            Log::new(Some(&log))
                                .warn(format!("ActivityPub client {line} refused: {why}"));
                        }
                        Ok::<_, Infallible>(reply)
                    }
                });
                let _ = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(ap::HEADER_TIMEOUT)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            };
            let handle = tokio::spawn(conn);
            endpoint_state.register_client_task(client_id, handle).await;
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, endpoint)
        .await;

    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<(Value, usize)>(32);
    let (event_tx, mut event_rx) = mpsc::channel::<(Event, usize)>(64);
    event_tx.try_send((
        Event::new(
            &actions::READY_EVENT,
            json!({"actor_id": actor_id, "handle": instance.handle(&username), "remote": ctx.remote_addr}),
        ),
        0,
    ))?;

    // Verified inbox activities become events, after Rust's own bookkeeping.
    let inbox_instance = instance.clone();
    let inbox_events = event_tx.clone();
    let inbox = tokio::spawn(async move {
        while let Some(inb) = inbound_rx.recv().await {
            ap::bookkeeping(&inbox_instance, &inb);
            let mut data = ap::event_data(&inbox_instance, &inb).await;
            if let Some(o) = data.as_object_mut() {
                o.remove("to_actor");
            }
            if inbox_events
                .send((Event::new(&actions::ACTIVITY_EVENT, data), 0))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    ctx.state.register_client_task(ctx.client_id, inbox).await;

    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        while let Some((event, depth)) = event_rx.recv().await {
            events_ctx
                .state
                .record_access_log(
                    AccessLogOwner::Client(events_ctx.client_id.as_u32()),
                    "ActivityPub",
                    None,
                    event.id(),
                    event.data.clone(),
                    vec![],
                )
                .await;
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
            match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &ActivityPubClientProtocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("ActivityPub client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        session(
            &session_ctx,
            &instance,
            &username,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        dispatcher_abort.abort();
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

async fn perform(instance: &Instance, me: &str, action: &Value) -> Result<Value> {
    match action["type"].as_str().unwrap_or_default() {
        actions::LOOKUP => {
            let id = instance
                .resolve(action["target"].as_str().unwrap_or_default())
                .await?;
            let doc = instance.fetch(&id, Some(me)).await?;
            Ok(
                json!({"id": doc["id"], "type": doc["type"], "preferred_username": doc["preferredUsername"],
                      "name": doc["name"], "summary": doc["summary"].as_str().map(ap::instance::plain_text),
                      "inbox": doc["inbox"], "followers": doc["followers"],
                      "manually_approves_followers": doc["manuallyApprovesFollowers"]}),
            )
        }
        actions::FETCH => {
            instance
                .fetch(action["url"].as_str().unwrap_or_default(), Some(me))
                .await
        }
        _ => ap::apply_shared(instance, me, action, None).await,
    }
}

async fn session(
    ctx: &ConnectContext,
    instance: &Instance,
    me: &str,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<(Value, usize)>,
    events: mpsc::Sender<(Event, usize)>,
) {
    let log = Log::new(Some(&ctx.status_tx));
    loop {
        let (action, depth, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), 0, Some(c)),
                None => return,
            },
            action = internal.recv() => match action {
                Some((a, depth)) => (a, depth, None),
                None => return,
            },
        };
        let reply = |injected: &mut Option<ClientCommand>, outcome: ClientSendOutcome| {
            if let Some(command) = injected.take() {
                crate::client::command_support::reply(command, Ok(outcome));
            }
        };
        match ActivityPubClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(&mut injected, ClientSendOutcome::Disconnected);
                return;
            }
            Ok(_) => {}
            Err(e) => {
                log.warn(format!("ActivityPub client action refused: {e:#}"));
                reply(
                    &mut injected,
                    ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    },
                );
                continue;
            }
        }
        if depth > MAX_FOLLOWUP_DEPTH {
            log.warn(format!(
                "ActivityPub client: handler chain stopped after {MAX_FOLLOWUP_DEPTH} follow-ups"
            ));
            continue;
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "ActivityPub",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![],
                )
                .await;
        }
        let operation = action["type"].as_str().unwrap_or_default();
        let data = match perform(instance, me, &action).await {
            Ok(result) => json!({"operation": operation, "ok": true, "result": result}),
            Err(e) => json!({"operation": operation, "ok": false, "error": format!("{e:#}")}),
        };
        reply(
            &mut injected,
            ClientSendOutcome::Executed {
                detail: data.to_string(),
            },
        );
        if events
            .try_send((Event::new(&actions::RESPONSE_EVENT, data), depth))
            .is_err()
        {
            log.warn("ActivityPub client: event queue full; consumer stalled".to_string());
            return;
        }
    }
}
