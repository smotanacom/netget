//! RDAP client: one structured query at a time against a fixed base URL.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::rdap::query::{self, Query};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RdapClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const TIMEOUT: Duration = Duration::from_secs(15);
/// Plain HTTP unless `https` is set.
pub const DEFAULT_HTTPS: bool = false;

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let base_path = p
        .map(|p| p.get_optional_string("base_path"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| query::DEFAULT_BASE_PATH.into());
    ensure!(
        base_path.starts_with('/')
            && base_path.len() <= 128
            && !base_path.contains(['?', '#', ' ']),
        "base_path must be an absolute path"
    );
    let https = p
        .map(|p| p.get_optional_bool("https"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_HTTPS);
    let timeout = Duration::from_secs(
        p.map(|p| p.get_optional_u64("timeout_secs"))
            .transpose()?
            .flatten()
            .unwrap_or(TIMEOUT.as_secs()),
    );
    ensure!(
        (1..=120).contains(&timeout.as_secs()),
        "timeout_secs must be 1..=120"
    );
    let base_url = format!(
        "{}://{}{}/",
        if https { "https" } else { "http" },
        ctx.remote_addr,
        base_path.trim_end_matches('/')
    );
    crate::client::http_fetch::check_url(&base_url)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none()),
            &base_url,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(timeout);
    let fetch = fetch
        .with_max_body(query::MAX_BODY_BYTES)
        .with_user_agent("netget-rdap");
    // RDAP has no session to open; resolve the target once so a bad address fails now.
    let local: SocketAddr = match tokio::net::lookup_host(&ctx.remote_addr).await?.next() {
        Some(addr) if addr.is_ipv4() => "0.0.0.0:0".parse()?,
        Some(_) => "[::]:0".parse()?,
        None => anyhow::bail!("RDAP server address {} did not resolve", ctx.remote_addr),
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::READY_EVENT,
        json!({"base_url": base_url}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = RdapClientProtocol;
        while let Some(event) = event_rx.recv().await {
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
                &protocol,
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
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("RDAP client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            &fetch,
            &base_url,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("RDAP client ended: {e}"));
                ClientStatus::Error(e.to_string())
            }
        };
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, status)
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

async fn session(
    ctx: &ConnectContext,
    fetch: &FetchClient,
    base_url: &str,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let query = match RdapClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(ClientActionResult::Custom { data, .. }) => Query::from_action(&data),
            Ok(_) => Err(anyhow::anyhow!("unsupported action")),
            Err(e) => Err(e),
        };
        let query = match query {
            Ok(q) => q,
            Err(e) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        };
        let url = format!("{base_url}{}", query.path());
        let outcome = exchange(fetch, &query, &url).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "RDAP",
                    None,
                    "injected_action",
                    action,
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Sent {
                            bytes_sent: url.len(),
                        }),
                    );
                }
                events
                    .try_send(Event::new(&actions::RESPONSE_EVENT, data))
                    .context("RDAP event queue full; consumer stalled")?;
            }
            Err(e) => {
                // A transport failure or a malformed answer is reported and the client stays up.
                Log::new(Some(&ctx.status_tx))
                    .warn(format!("RDAP query {} failed: {e}", query.path()));
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Err(e));
                }
            }
        }
    }
}

async fn exchange(fetch: &FetchClient, query: &Query, url: &str) -> Result<Value> {
    let response = fetch
        .get(url)
        .header("Accept", "application/rdap+json, application/json;q=0.9")
        .send()
        .await?;
    let status = response.status().as_u16();
    let location = response
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let body = response.bytes().await?;
    let mut data = query.to_event();
    data["status"] = json!(status);
    if (300..400).contains(&status) {
        data["redirect"] = json!(location.context("redirect without Location")?);
        return Ok(data);
    }
    let json_type =
        content_type.starts_with(query::MEDIA_TYPE) || content_type.starts_with("application/json");
    if status == 200 {
        ensure!(
            json_type,
            "200 response is not RDAP JSON (Content-Type {content_type:?})"
        );
        let value: Value = serde_json::from_slice(&body).context("200 response is not JSON")?;
        query::check_response(query, &value)?;
        data["object"] = value;
    } else if json_type && !body.is_empty() {
        let value: Value = serde_json::from_slice(&body).context("error response is not JSON")?;
        ensure!(query::json_ok(&value), "error response exceeds bounds");
        data["error"] = value;
    }
    Ok(data)
}
