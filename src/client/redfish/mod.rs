//! DMTF Redfish client.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::redfish::model;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RedfishClientProtocol;
use anyhow::{ensure, Context, Result};
use base64::Engine;
use hyper::Method;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_SCHEME: &str = "https";
pub const DEFAULT_AUTH_METHOD: &str = "session";
pub const DEFAULT_INSECURE: bool = false;
const TIMEOUT: Duration = Duration::from_secs(30);
const TASK_FOLLOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
enum Auth {
    None,
    Basic(String),
    Session {
        token: String,
        location: Option<String>,
    },
}

struct Conn {
    fetch: FetchClient,
    base: String,
    auth: Auth,
}

impl Conn {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn request(&self, method: Method, path: &str) -> crate::client::http_fetch::FetchRequest {
        let r = self
            .fetch
            .request(method, &self.url(path))
            .header("OData-Version", "4.0")
            .header("Accept", "application/json");
        match &self.auth {
            Auth::None => r,
            Auth::Basic(b) => r.header("Authorization", format!("Basic {b}")),
            Auth::Session { token, .. } => r.header("X-Auth-Token", token),
        }
    }

    /// One request: `(status, body, location)`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        if_match: Option<&str>,
    ) -> Result<(u16, Option<Value>, Option<String>)> {
        let mut r = self.request(method, path);
        if let Some(b) = body {
            r = r
                .header("Content-Type", "application/json")
                .body(b.to_string());
        }
        if let Some(e) = if_match {
            r = r.header("If-Match", e);
        }
        let response = r.send().await?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(location_path);
        let bytes = response.bytes().await?;
        let body = if bytes.is_empty() {
            None
        } else {
            let v: Value =
                serde_json::from_slice(&bytes).context("Redfish response is not JSON")?;
            ensure!(model::budget_ok(&v), "Redfish response exceeds the bounds");
            Some(v)
        };
        Ok((status, body, location))
    }
}

/// A Location header as a path (services may send an absolute URL).
fn location_path(v: &str) -> String {
    match v.split_once("://") {
        Some((_, rest)) => rest
            .find('/')
            .map(|i| rest[i..].to_owned())
            .unwrap_or_else(|| "/".into()),
        None => v.to_owned(),
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
    let method = opt("auth_method")?.unwrap_or_else(|| DEFAULT_AUTH_METHOD.to_owned());
    ensure!(
        method == "session" || method == "basic",
        "auth_method must be session or basic"
    );
    let insecure = p
        .map(|p| p.get_optional_bool("insecure"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_INSECURE);
    let (user, password) = (opt("username")?, opt("password")?);
    let base = format!("{scheme}://{}", ctx.remote_addr);
    crate::client::http_fetch::check_url(&base)?;
    #[cfg(not(target_arch = "wasm32"))]
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .danger_accept_invalid_certs(insecure),
            &base,
        )
        .build()?,
    );
    #[cfg(target_arch = "wasm32")]
    let fetch = {
        let _ = insecure;
        FetchClient::transport(TIMEOUT)
    };
    let mut conn = Conn {
        fetch: fetch
            .with_max_body(model::MAX_BODY_BYTES)
            .with_user_agent("netget-redfish"),
        base,
        auth: Auth::None,
    };
    let (status, root, _) = conn
        .send(Method::GET, model::ROOT, None, None)
        .await
        .context("reading the Redfish service root")?;
    ensure!(status == 200, "service root answered HTTP {status}");
    let root = root.context("service root has no body")?;
    ensure!(
        root["@odata.type"]
            .as_str()
            .is_some_and(|t| t.starts_with("#ServiceRoot.")),
        "/redfish/v1/ is not a ServiceRoot"
    );
    let mut authenticated = "none";
    if let Some(user) = user {
        let password = password.unwrap_or_default();
        if method == "basic" {
            conn.auth = Auth::Basic(
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")),
            );
            authenticated = "basic";
        } else {
            let sessions = root["Links"]["Sessions"]["@odata.id"]
                .as_str()
                .and_then(model::normalize)
                .unwrap_or_else(|| "/redfish/v1/SessionService/Sessions".into());
            let response = conn
                .request(Method::POST, &sessions)
                .header("Content-Type", "application/json")
                .body(json!({"UserName": user, "Password": password}).to_string())
                .send()
                .await
                .context("creating a Redfish session")?;
            let status = response.status().as_u16();
            ensure!(
                (200..300).contains(&status),
                "session login refused with HTTP {status}"
            );
            let token = response
                .headers()
                .get("x-auth-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .context("session login returned no X-Auth-Token")?;
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(location_path);
            conn.auth = Auth::Session { token, location };
            authenticated = "session";
        }
    }
    let links: Map<String, Value> = root
        .as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v["@odata.id"].as_str().map(|p| (k.clone(), json!(p))))
                .collect()
        })
        .unwrap_or_default();
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
        json!({"redfish_version": root["RedfishVersion"], "product": root["Product"], "vendor": root["Vendor"], "links": links, "authenticated": authenticated}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = RedfishClientProtocol;
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
                    .warn(format!("Redfish client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, &conn, external, internal_rx, event_tx).await;
        if let Auth::Session {
            location: Some(l), ..
        } = &conn.auth
        {
            let _ = conn.send(Method::DELETE, l, None, None).await;
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Redfish client ended: {e}"));
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
        match RedfishClientProtocol.execute_action(action.clone()) {
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
                    "Redfish",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "path": action.get("path").or(action.get("resource_path"))}),
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
                    .context("Redfish event consumer stopped")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("Redfish request failed: {e}"));
                reply(command, Err(e));
            }
        }
    }
}

async fn perform(conn: &Conn, action: &Value) -> Result<Value> {
    let (method, path, body) = match action["type"].as_str().unwrap_or_default() {
        "redfish_get" => (
            Method::GET,
            action["path"].as_str().unwrap_or_default().to_owned(),
            None,
        ),
        "redfish_delete" => (
            Method::DELETE,
            action["path"].as_str().unwrap_or_default().to_owned(),
            None,
        ),
        "redfish_patch" => (
            Method::PATCH,
            action["path"].as_str().unwrap_or_default().to_owned(),
            Some(action["body"].clone()),
        ),
        "redfish_post" => (
            Method::POST,
            action["path"].as_str().unwrap_or_default().to_owned(),
            Some(action["body"].clone()),
        ),
        _ => {
            let resource_path = action["resource_path"].as_str().unwrap_or_default();
            let name = action["action"].as_str().unwrap_or_default();
            let (status, resource, _) = conn.send(Method::GET, resource_path, None, None).await?;
            ensure!(
                status == 200,
                "reading {resource_path} answered HTTP {status}"
            );
            let target = resource
                .as_ref()
                .and_then(|r| model::action_target(r, name))
                .with_context(|| format!("{resource_path} does not advertise #{name}"))?;
            (
                Method::POST,
                target,
                Some(
                    action
                        .get("parameters")
                        .filter(|p| p.is_object())
                        .cloned()
                        .unwrap_or(json!({})),
                ),
            )
        }
    };
    let (mut status, mut payload, location) = conn
        .send(
            method.clone(),
            &path,
            body.as_ref(),
            action["if_match"].as_str(),
        )
        .await?;
    let mut data = json!({"method": method.as_str(), "path": path, "status": status});
    if let Some(l) = &location {
        data["location"] = json!(l);
    }
    // A 202 hands back a task monitor: follow it until the operation's own answer arrives.
    if status == 202 {
        if let Some(monitor) = location.clone().or_else(|| {
            payload
                .as_ref()
                .and_then(|p| p["TaskMonitor"].as_str().map(str::to_owned))
        }) {
            let deadline = tokio::time::Instant::now() + TASK_FOLLOW;
            let mut last_task = payload.clone();
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let (s, p, _) = conn.send(Method::GET, &monitor, None, None).await?;
                if s != 202 {
                    let task_uri = last_task
                        .as_ref()
                        .and_then(|t| t["@odata.id"].as_str())
                        .map(str::to_owned);
                    let task = match task_uri {
                        Some(u) => conn
                            .send(Method::GET, &u, None, None)
                            .await
                            .ok()
                            .and_then(|(_, b, _)| b),
                        None => None,
                    };
                    data["task"] = json!({"monitor": monitor, "state": task.as_ref().map(|t| t["TaskState"].clone()), "messages": task.as_ref().map(|t| t["Messages"].clone())});
                    status = s;
                    payload = p;
                    break;
                }
                last_task = p;
                if tokio::time::Instant::now() >= deadline {
                    data["task"] = json!({"monitor": monitor, "state": "Running", "error": "still running after 60 s"});
                    payload = last_task;
                    break;
                }
            }
            data["status"] = json!(status);
        }
    }
    if let Some(p) = payload {
        if status >= 400 {
            let e = &p["error"];
            let mid = e["@Message.ExtendedInfo"][0]["MessageId"]
                .as_str()
                .or_else(|| e["code"].as_str())
                .unwrap_or("");
            data["error"] = json!({"message_id": mid, "message": e["message"]});
        }
        data["body"] = p;
    }
    Ok(data)
}
