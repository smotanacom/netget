//! JMAP client: fetches the session, then sends the handler's batched requests to its apiUrl.
pub mod actions;
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::JmapClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_SESSION_PATH: &str = "/.well-known/jmap";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 4 * 1024 * 1024;
/// handler → request → response → handler … stops here; an injected request starts afresh.
const MAX_FOLLOWUP_DEPTH: usize = 8;

enum Auth {
    None,
    Basic(String, String),
    Bearer(String),
}

struct Conn {
    fetch: FetchClient,
    auth: Auth,
    api_url: String,
    primary: Map<String, Value>,
    state: String,
}

fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    Some(format!("{scheme}://{}", rest.split('/').next()?))
}

impl Conn {
    fn authorize(
        &self,
        req: crate::client::http_fetch::FetchRequest,
    ) -> crate::client::http_fetch::FetchRequest {
        match &self.auth {
            Auth::None => req,
            Auth::Basic(u, p) => req.basic_auth(u, Some(p)),
            Auth::Bearer(t) => req.header("authorization", format!("Bearer {t}")),
        }
    }

    async fn send(&self, v: &Value) -> Result<Value> {
        let (using, calls) = actions::prepare(v, &self.primary)?;
        let body = json!({"using": using, "methodCalls": calls});
        let resp = self
            .authorize(self.fetch.post(&self.api_url))
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await?;
        let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        if status != 200 {
            return Ok(json!({"status": status, "problem": parsed}));
        }
        let session_state = parsed["sessionState"].as_str().unwrap_or_default();
        Ok(json!({
            "status": status,
            "method_responses": parsed["methodResponses"],
            "session_state": session_state,
            "session_changed": session_state != self.state,
        }))
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let auth = match (get("username")?, get("password")?, get("api_token")?) {
        (Some(u), Some(pw), None) => Auth::Basic(u, pw),
        (None, None, Some(t)) => Auth::Bearer(t),
        (None, None, None) => Auth::None,
        _ => bail!("give username and password, or api_token"),
    };
    let tls = p
        .map(|p| p.get_optional_bool("tls"))
        .transpose()?
        .flatten()
        .unwrap_or(true);
    let session_path = get("session_path")?.unwrap_or_else(|| DEFAULT_SESSION_PATH.into());
    ensure!(
        session_path.starts_with('/') && !session_path.contains("..") && session_path.len() <= 256,
        "session_path is an absolute path"
    );
    let remote: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await?
        .next()
        .context("the address does not resolve")?;
    let base = format!(
        "{}://{}",
        if tls { "https" } else { "http" },
        ctx.remote_addr
    );
    crate::client::http_fetch::check_url(&base)?;
    let first = origin(&base).context("a base URL")?;
    let same_origin = first.clone();
    let mut builder =
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5
                    || origin(attempt.url().as_str()).as_deref() != Some(same_origin.as_str())
                {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }));
    if let Some(ca) = get("ca_cert_path")? {
        let pem = tokio::fs::read(&ca)
            .await
            .with_context(|| format!("reading {ca}"))?;
        // Only this certificate is trusted, verified by rustls itself: the platform verifier
        // refuses a self-signed server certificate as its own anchor.
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            roots.add(cert.with_context(|| format!("{ca} holds no PEM certificate"))?)?;
        }
        ensure!(!roots.is_empty(), "{ca} holds no PEM certificate");
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        builder = builder.use_preconfigured_tls(tls);
    }
    let fetch = FetchClient::from_reqwest(
        crate::llm::ollama_client::configured_for_endpoint(builder, &base).build()?,
    )
    .with_max_body(MAX_BODY)
    .with_user_agent("netget-jmap");
    let mut conn = Conn {
        fetch,
        auth,
        api_url: String::new(),
        primary: Map::new(),
        state: String::new(),
    };
    let resp = conn
        .authorize(conn.fetch.get(&format!("{base}{session_path}")))
        .header("accept", "application/json")
        .send()
        .await?;
    let status = resp.status().as_u16();
    ensure!(status == 200, "the session resource answered HTTP {status}");
    let session: Value =
        serde_json::from_slice(&resp.bytes().await?).context("the session is not JSON")?;
    let api_url = session["apiUrl"]
        .as_str()
        .context("the session has no apiUrl")?;
    // A session that sends requests somewhere else is not the server this client was pointed at.
    ensure!(
        origin(api_url).as_deref() == Some(first.as_str()),
        "the session's apiUrl {api_url} is not on {first}"
    );
    conn.api_url = api_url.to_owned();
    conn.primary = session["primaryAccounts"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    conn.state = session["state"].as_str().unwrap_or_default().to_owned();
    let accounts: Vec<Value> = session["accounts"]
        .as_object()
        .map(|m| {
            m.iter()
                .take(64)
                .map(|(id, a)| json!({"id": id, "name": a["name"], "capabilities": a["accountCapabilities"].as_object().map(|c| c.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()}))
                .collect()
        })
        .unwrap_or_default();
    let capabilities: Vec<String> = session["capabilities"]
        .as_object()
        .map(|c| c.keys().take(64).cloned().collect())
        .unwrap_or_default();
    let info = json!({"username": session["username"], "accounts": accounts, "primary_accounts": conn.primary, "capabilities": capabilities, "state": conn.state});
    Log::new(Some(&ctx.status_tx)).info(format!(
        "JMAP session at {} for {}",
        conn.api_url, session["username"]
    ));
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, info))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = JmapClientProtocol;
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
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("JMAP client handler: {e}"))
                }
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
    mut internal: mpsc::Receiver<Value>,
    events: &mpsc::Sender<Event>,
) {
    let mut depth = 0usize;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return },
            a = internal.recv() => match a { Some(a) => (a, None), None => return },
        };
        let outcome = match JmapClientProtocol.execute_action(action.clone()) {
            Err(e) => ClientSendOutcome::Rejected {
                error: e.to_string(),
            },
            Ok(ClientActionResult::Disconnect) => {
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return;
            }
            Ok(_) => match conn.send(&action).await {
                Ok(response) => {
                    depth = if command.is_some() { 0 } else { depth + 1 };
                    if depth <= MAX_FOLLOWUP_DEPTH {
                        events
                            .send(Event::new(&actions::RESPONSE_EVENT, response))
                            .await
                            .ok();
                    } else {
                        Log::new(Some(&ctx.status_tx)).warn(format!(
                            "JMAP client: follow-up depth {MAX_FOLLOWUP_DEPTH} reached; the response is not raised"
                        ));
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
                    "JMAP",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                )
                .await;
            crate::client::command_support::reply(c, Ok(outcome));
        }
    }
}
