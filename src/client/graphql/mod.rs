//! GraphQL over HTTP client.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::graphql::{engine, GRAPHQL_RESPONSE};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::GraphqlClientProtocol;
use anyhow::{ensure, Context, Result};
use apollo_compiler::executable::OperationType;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_INTROSPECT: bool = true;
const TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT: &str = "application/graphql-response+json, application/json;q=0.9";

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let endpoint = p
        .map(|p| p.get_optional_string("endpoint"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| crate::server::graphql::DEFAULT_ENDPOINT.to_owned());
    ensure!(
        endpoint.starts_with('/') && endpoint.len() <= 256 && !endpoint.contains(['#', ' ']),
        "endpoint must be an absolute path"
    );
    let introspect = p
        .map(|p| p.get_optional_bool("introspect"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_INTROSPECT);
    let url = format!("http://{}{endpoint}", ctx.remote_addr);
    crate::client::http_fetch::check_url(&url)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none()),
            &url,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(TIMEOUT);
    let fetch = fetch
        .with_max_body(engine::MAX_BODY_BYTES)
        .with_user_agent("netget-graphql");
    let mut connected = json!({"url": url});
    if introspect {
        match exchange(
            &fetch,
            &url,
            engine::CLIENT_INTROSPECTION,
            None,
            &json!({}),
            false,
        )
        .await
        {
            Ok(r) if r["data"].is_object() && r.get("errors").is_none() => {
                connected["root_fields"] = engine::root_signatures(&r["data"]);
            }
            Ok(r) => {
                connected["introspection_error"] = json!(r
                    .get("error")
                    .or_else(|| r["errors"].get(0).map(|e| &e["message"]))
                    .and_then(Value::as_str)
                    .unwrap_or("introspection gave no schema"))
            }
            Err(e) => bail_connect(&e)?,
        }
    }
    let local: SocketAddr = "0.0.0.0:0".parse()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, connected))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = GraphqlClientProtocol;
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
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("GraphQL client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &fetch, &url, external, internal_rx, event_tx).await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("GraphQL client ended: {e}"));
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

/// A transport failure on the first request means there is no server to talk to.
fn bail_connect(e: &anyhow::Error) -> Result<()> {
    Err(anyhow::anyhow!("GraphQL endpoint unreachable: {e}"))
}

async fn session(
    ctx: &ConnectContext,
    fetch: &FetchClient,
    url: &str,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let checked = match GraphqlClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => engine::parse_operation(
                action["query"].as_str().unwrap_or_default(),
                action["operation_name"].as_str(),
            ),
            Err(e) => Err(e),
        };
        let (op_type, op_name) = match checked {
            Ok(v) => v,
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
        let use_get = action["use_get"].as_bool().unwrap_or(false);
        if use_get && op_type == OperationType::Mutation {
            if let Some(c) = command {
                crate::client::command_support::reply(
                    c,
                    Ok(ClientSendOutcome::Rejected {
                        error: "mutations cannot be sent with GET".into(),
                    }),
                );
            }
            continue;
        }
        let query = action["query"].as_str().unwrap_or_default();
        let variables = action.get("variables").cloned().unwrap_or(json!({}));
        let outcome = exchange(
            fetch,
            url,
            query,
            action["operation_name"].as_str(),
            &variables,
            use_get,
        )
        .await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "GraphQL",
                    None,
                    "injected_action",
                    json!({"operation_type": engine::operation_type_name(op_type)}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(mut data) => {
                data["operation_type"] = json!(engine::operation_type_name(op_type));
                data["operation_name"] = json!(op_name);
                if let Some(c) = command {
                    crate::client::command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Sent {
                            bytes_sent: query.len(),
                        }),
                    );
                }
                events
                    .try_send(Event::new(&actions::RESPONSE_EVENT, data))
                    .context("GraphQL event queue full; consumer stalled")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("GraphQL request failed: {e}"));
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Err(e));
                }
            }
        }
    }
}

/// One GraphQL-over-HTTP request. Transport failures are `Err`; any HTTP answer is `Ok` with
/// either the checked GraphQL body or an `error` saying why it was not one.
async fn exchange(
    fetch: &FetchClient,
    url: &str,
    query: &str,
    operation_name: Option<&str>,
    variables: &Value,
    use_get: bool,
) -> Result<Value> {
    let request = if use_get {
        let mut params = vec![("query", query.to_owned())];
        if let Some(n) = operation_name {
            params.push(("operationName", n.to_owned()));
        }
        if variables.as_object().is_some_and(|v| !v.is_empty()) {
            params.push(("variables", variables.to_string()));
        }
        fetch.get(url).query(&params)
    } else {
        let mut body = json!({"query": query});
        if let Some(n) = operation_name {
            body["operationName"] = json!(n);
        }
        if variables.as_object().is_some_and(|v| !v.is_empty()) {
            body["variables"] = variables.clone();
        }
        fetch
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.to_string())
    };
    let response = request.header("Accept", ACCEPT).send().await?;
    let status = response.status().as_u16();
    let media = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let bytes = response.bytes().await?;
    let mut out = json!({"status": status});
    if media != GRAPHQL_RESPONSE && media != "application/json" {
        out["error"] = json!(format!(
            "HTTP {status} answered with {} rather than a GraphQL response",
            if media.is_empty() {
                "no content type"
            } else {
                &media
            }
        ));
        return Ok(out);
    }
    out["media_type"] = json!(media);
    let body = match serde_json::from_slice::<Value>(&bytes) {
        Ok(b) if engine::budget_ok(&b) => b,
        _ => {
            out["error"] = json!(format!(
                "HTTP {status}: body is not a bounded JSON document"
            ));
            return Ok(out);
        }
    };
    if let Err(e) = engine::check_response(&body) {
        out["error"] = json!(format!("HTTP {status}: not a GraphQL response: {e}"));
        return Ok(out);
    }
    // graphql-response+json: a body without data must not come with 2xx.
    if media == GRAPHQL_RESPONSE && body.get("data").is_none() && (200..300).contains(&status) {
        out["error"] = json!("a response without data was sent with a 2xx status");
        return Ok(out);
    }
    for k in ["data", "errors", "extensions"] {
        if let Some(v) = body.get(k) {
            out[k] = v.clone();
        }
    }
    Ok(out)
}
