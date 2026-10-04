//! SCIM 2.0 client.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::scim::{model_bounds, schema};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ScimClientProtocol;
use anyhow::{ensure, Context, Result};
use hyper::Method;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_SCHEME: &str = "https";
const TIMEOUT: Duration = Duration::from_secs(30);
const SCIM_JSON: &str = "application/scim+json";

struct Conn {
    fetch: FetchClient,
    base: String,
    token: Option<String>,
}

impl Conn {
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<(u16, Option<Value>)> {
        let mut r = self
            .fetch
            .request(method, &format!("{}{path}", self.base))
            .header("Accept", SCIM_JSON);
        if !query.is_empty() {
            r = r.query(query);
        }
        if let Some(t) = &self.token {
            r = r.header("Authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            r = r.header("Content-Type", SCIM_JSON).body(b.to_string());
        }
        let response = r.send().await?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Ok((status, None));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("HTTP {status}: body is not JSON"))?;
        ensure!(
            model_bounds::budget_ok(&v),
            "answer exceeds the SCIM bounds"
        );
        Ok((status, Some(v)))
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let opt = |k: &str| -> Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let scheme = opt("scheme")?.unwrap_or_else(|| DEFAULT_SCHEME.to_owned());
    ensure!(
        scheme == "http" || scheme == "https",
        "scheme must be http or https"
    );
    let base_path =
        opt("base_path")?.unwrap_or_else(|| crate::server::scim::DEFAULT_BASE_PATH.to_owned());
    let base_path = base_path.trim_end_matches('/').to_owned();
    ensure!(
        base_path.is_empty() || base_path.starts_with('/'),
        "base_path must be empty or absolute"
    );
    let base = format!("{scheme}://{}{base_path}", ctx.remote_addr);
    crate::client::http_fetch::check_url(&base)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none()),
            &base,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = FetchClient::transport(TIMEOUT);
    let conn = Conn {
        fetch: fetch
            .with_max_body(model_bounds::MAX_BODY_BYTES)
            .with_user_agent("netget-scim"),
        base,
        token: opt("bearer_token")?,
    };
    let (status, spc) = conn
        .send(Method::GET, "/ServiceProviderConfig", &[], None)
        .await
        .context("reading ServiceProviderConfig")?;
    ensure!(
        status == 200,
        "ServiceProviderConfig answered HTTP {status}"
    );
    let spc = spc.context("ServiceProviderConfig has no body")?;
    let (status, rts) = conn
        .send(Method::GET, "/ResourceTypes", &[], None)
        .await
        .context("reading ResourceTypes")?;
    ensure!(status == 200, "ResourceTypes answered HTTP {status}");
    let rts = rts.context("ResourceTypes has no body")?;
    let list = rts["Resources"]
        .as_array()
        .or_else(|| rts.as_array())
        .cloned()
        .unwrap_or_default();
    let resource_types: Vec<Value> = list
        .iter()
        .map(|r| json!({"name": r["name"], "endpoint": r["endpoint"], "schema": r["schema"]}))
        .collect();
    let supported = |k: &str| spc[k]["supported"].as_bool().unwrap_or(false);
    let features = json!({"patch": supported("patch"), "filter": supported("filter"), "sort": supported("sort"), "bulk": supported("bulk"), "etag": supported("etag"), "changePassword": supported("changePassword")});
    let local: SocketAddr = "0.0.0.0:0".parse()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"resource_types": resource_types, "features": features}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = ScimClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("SCIM client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &conn, external, internal_rx, event_tx).await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("SCIM client ended: {e}"));
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

fn reply(command: Option<ClientCommand>, outcome: Result<ClientSendOutcome>) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, outcome);
    }
}

async fn session(
    ctx: &ConnectContext,
    conn: &Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        match ScimClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(command, Ok(ClientSendOutcome::Disconnected));
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                reply(
                    command,
                    Ok(ClientSendOutcome::Rejected {
                        error: e.to_string(),
                    }),
                );
                continue;
            }
        }
        let outcome = perform(conn, &action).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "SCIM",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "resource_type": action["resource_type"]}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                reply(command, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                events
                    .send(Event::new(&actions::RESPONSE_EVENT, data))
                    .await
                    .context("SCIM event consumer stopped")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("SCIM request failed: {e}"));
                reply(command, Err(e));
            }
        }
    }
}

fn with_schema(resource: &Value, rt: &str) -> Value {
    let mut r = resource.clone();
    if r.get("schemas").is_none() {
        let urn = match rt {
            "Users" => Some(schema::USER),
            "Groups" => Some(schema::GROUP),
            _ => None,
        };
        if let Some(u) = urn {
            r["schemas"] = json!([u]);
        }
    }
    r
}

async fn perform(conn: &Conn, a: &Value) -> Result<Value> {
    let rt = a["resource_type"].as_str().unwrap_or_default();
    let id = a["id"].as_str().unwrap_or_default();
    let item = format!("/{rt}/{}", urlencoding::encode(id));
    let (operation, (status, body)) = match a["type"].as_str().unwrap_or_default() {
        "scim_list" => {
            let mut q: Vec<(&str, String)> = Vec::new();
            for (param, key) in [
                ("filter", "filter"),
                ("sortBy", "sort_by"),
                ("sortOrder", "sort_order"),
                ("attributes", "attributes"),
            ] {
                if let Some(v) = a[key].as_str() {
                    q.push((param, v.to_owned()));
                }
            }
            for (param, key) in [("startIndex", "start_index"), ("count", "count")] {
                if let Some(v) = a[key].as_u64() {
                    q.push((param, v.to_string()));
                }
            }
            (
                "list",
                conn.send(Method::GET, &format!("/{rt}"), &q, None).await?,
            )
        }
        "scim_get" => ("get", conn.send(Method::GET, &item, &[], None).await?),
        "scim_delete" => ("delete", conn.send(Method::DELETE, &item, &[], None).await?),
        "scim_create" => (
            "create",
            conn.send(
                Method::POST,
                &format!("/{rt}"),
                &[],
                Some(&with_schema(&a["resource"], rt)),
            )
            .await?,
        ),
        "scim_replace" => (
            "replace",
            conn.send(
                Method::PUT,
                &item,
                &[],
                Some(&with_schema(&a["resource"], rt)),
            )
            .await?,
        ),
        _ => (
            "patch",
            conn.send(
                Method::PATCH,
                &item,
                &[],
                Some(&json!({"schemas": [schema::PATCH_OP], "Operations": a["operations"]})),
            )
            .await?,
        ),
    };
    let mut data = json!({"operation": operation, "status": status});
    if let Some(b) = body {
        if status >= 400 {
            let s = b["status"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| b["status"].as_u64());
            data["error"] = json!({"status": s, "scim_type": b["scimType"], "detail": b["detail"]});
        } else if operation == "list" {
            data["total_results"] = b["totalResults"].clone();
            data["resources"] = b["Resources"].clone();
        } else {
            data["resource"] = b;
        }
    }
    Ok(data)
}
