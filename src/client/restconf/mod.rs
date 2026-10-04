//! RESTCONF client over the shared HTTP fetch client.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::RestconfClientProtocol;
use anyhow::{Context, Result};
use hyper::Method;
use serde_json::{json, Value as Json};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(30);
const MEDIA: &str = "application/yang-data+json";
const MAX_BODY: usize = 4 * 1024 * 1024;

struct Conn {
    fetch: FetchClient,
    base: String,
    root: String,
    auth: Option<(String, String)>,
}

impl Conn {
    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<&Json>,
    ) -> Result<(u16, Option<Json>, Option<String>)> {
        let mut req = self.fetch.request(method, url).header("accept", MEDIA);
        if let Some((u, p)) = &self.auth {
            req = req.basic_auth(u, Some(p));
        }
        if let Some(b) = body {
            req = req
                .header("content-type", MEDIA)
                .body(serde_json::to_vec(b)?);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|l| l.to_str().ok())
            .map(str::to_owned);
        let bytes = resp.bytes().await?;
        let parsed = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        Ok((status, parsed, location))
    }

    async fn discover(&mut self) -> Result<Json> {
        let resp = self
            .fetch
            .get(&format!("{}/.well-known/host-meta", self.base))
            .header("accept", "application/xrd+xml, application/json")
            .send()
            .await;
        if let Ok(r) = resp {
            if r.status().is_success() {
                let text = r.text().await.unwrap_or_default();
                let href = serde_json::from_str::<Json>(&text)
                    .ok()
                    .and_then(|j| {
                        j["links"]
                            .as_array()?
                            .iter()
                            .find(|l| l["rel"] == "restconf")?["href"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .or_else(|| {
                        let at = text
                            .find("rel='restconf'")
                            .or_else(|| text.find("rel=\"restconf\""))?;
                        let rest = &text[at..];
                        let h = rest.find("href=")? + 6;
                        let quote = rest.as_bytes().get(h - 1).copied()? as char;
                        Some(rest[h..].split(quote).next()?.to_owned())
                    });
                if let Some(h) = href.filter(|h| h.starts_with('/') && h.len() <= 256) {
                    self.root = h.trim_end_matches('/').to_owned();
                }
            }
        }
        let root = format!("{}{}", self.base, self.root);
        // The API root and yang-library-version are optional in practice (FreeCONF serves
        // neither); the YANG library is what a client needs.
        let (root_status, body, _) = self.send(Method::GET, &root, None).await?;
        let mut version = body
            .as_ref()
            .filter(|_| root_status == 200)
            .and_then(|b| b["ietf-restconf:restconf"]["yang-library-version"].as_str())
            .map(str::to_owned);
        if version.is_none() {
            if let Ok((200, Some(v), _)) = self
                .send(Method::GET, &format!("{root}/yang-library-version"), None)
                .await
            {
                version = v["ietf-restconf:yang-library-version"]
                    .as_str()
                    .map(str::to_owned);
            }
        }
        let modules = match self
            .send(
                Method::GET,
                &format!("{root}/data/ietf-yang-library:modules-state"),
                None,
            )
            .await
        {
            Ok((200, Some(v), _)) => {
                let state = v.get("ietf-yang-library:modules-state").unwrap_or(&v);
                state["module"].as_array().cloned().unwrap_or_default().into_iter().take(1024).map(|m| json!({"name": m["name"], "revision": m["revision"], "namespace": m["namespace"]})).collect()
            }
            _ => vec![],
        };
        Ok(
            json!({"root": self.root, "root_status": root_status, "yang_library_version": version, "modules": modules}),
        )
    }

    async fn act(&self, v: &Json) -> Result<Json> {
        let root = format!("{}{}", self.base, self.root);
        let path = v["path"].as_str().unwrap_or_default();
        let (method, url, body, label) = match v["type"].as_str().unwrap_or_default() {
            "restconf_get" => {
                let mut q = Vec::new();
                for k in ["depth", "content", "fields"] {
                    if let Some(x) = v[k].as_str() {
                        q.push(format!("{k}={}", urlencoding::encode(x)));
                    }
                }
                let suffix = if q.is_empty() {
                    String::new()
                } else {
                    format!("?{}", q.join("&"))
                };
                (
                    Method::GET,
                    format!("{root}/data/{path}{suffix}"),
                    None,
                    path.to_owned(),
                )
            }
            "restconf_put" => (
                Method::PUT,
                format!("{root}/data/{path}"),
                Some(v["data"].clone()),
                path.to_owned(),
            ),
            "restconf_post" => (
                Method::POST,
                format!("{root}/data/{path}"),
                Some(v["data"].clone()),
                path.to_owned(),
            ),
            "restconf_patch" => (
                Method::PATCH,
                format!("{root}/data/{path}"),
                Some(v["data"].clone()),
                path.to_owned(),
            ),
            "restconf_delete" => (
                Method::DELETE,
                format!("{root}/data/{path}"),
                None,
                path.to_owned(),
            ),
            "restconf_invoke" => {
                let op = v["operation"].as_str().unwrap_or_default();
                let module = op.split(':').next().unwrap_or_default();
                let body = v
                    .get("input")
                    .filter(|i| !i.is_null())
                    .map(|i| json!({format!("{module}:input"): i}));
                (
                    Method::POST,
                    format!("{root}/operations/{op}"),
                    body,
                    op.to_owned(),
                )
            }
            other => anyhow::bail!("{other} is not a RESTCONF request"),
        };
        let (status, data, location) = self.send(method.clone(), &url, body.as_ref()).await?;
        let mut out = json!({"method": method.as_str(), "path": label, "status": status});
        match data {
            Some(d) if status >= 400 => out["errors"] = d["ietf-restconf:errors"]["error"].clone(),
            Some(d) => out["data"] = d,
            None => {}
        }
        if let Some(l) = location {
            out["location"] = json!(l);
        }
        Ok(out)
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let user = p
        .map(|p| p.get_optional_string("username"))
        .transpose()?
        .flatten();
    let password = p
        .map(|p| p.get_optional_string("password"))
        .transpose()?
        .flatten();
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await?
        .next()
        .context("the address does not resolve")?;
    let base = format!("http://{}", ctx.remote_addr);
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
    let mut conn = Conn {
        fetch: fetch
            .with_max_body(MAX_BODY)
            .with_user_agent("netget-restconf"),
        base,
        root: "/restconf".into(),
        auth: user.zip(password),
    };
    let info = conn.discover().await?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, info))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = RestconfClientProtocol;
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
                    if let Some(m) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, m)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("RESTCONF client handler: {e}")),
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        run(&session_ctx, &conn, external, internal_rx, &event_tx).await;
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
    Ok(SocketAddr::new(remote.ip(), 0))
}

async fn run(
    ctx: &ConnectContext,
    conn: &Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
    events: &mpsc::Sender<Event>,
) {
    let mut depth = 0usize;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return },
            a = internal.recv() => match a { Some(a) => (a, None), None => return },
        };
        let outcome = match RestconfClientProtocol.execute_action(action.clone()) {
            Err(e) => ClientSendOutcome::Rejected {
                error: e.to_string(),
            },
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return;
            }
            Ok(_) => match conn.act(&action).await {
                Ok(response) => {
                    // A chain of handler-driven requests is bounded; injected ones reset it.
                    depth = if command.is_some() { 0 } else { depth + 1 };
                    if depth <= 8 {
                        events
                            .send(Event::new(&actions::RESPONSE_EVENT, response))
                            .await
                            .ok();
                    }
                    ClientSendOutcome::Sent { bytes_sent: 0 }
                }
                Err(e) => ClientSendOutcome::Rejected {
                    error: format!("{e:#}"),
                },
            },
        };
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "RESTCONF",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![serde_json::to_value(&outcome).unwrap_or(Json::Null)],
                )
                .await;
            crate::client::command_support::reply(c, Ok(outcome));
        }
    }
}
