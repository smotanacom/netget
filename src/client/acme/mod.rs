//! ACME (RFC 8555) client: one account key per client, one current order, an optional http-01
//! responder, and every request signed and nonce-checked here.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::acme::jws::{self, AccountKey};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::AcmeClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};

pub const DEFAULT_SCHEME: &str = "https";
pub const DEFAULT_DIRECTORY_PATH: &str = "/directory";
pub const MAX_RESPONSE: usize = 256 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
const POLLS: usize = 30;

type Tokens = Arc<Mutex<HashMap<String, String>>>;

struct Session {
    http: reqwest::Client,
    dir: Value,
    key: AccountKey,
    kid: Option<String>,
    nonce: Option<String>,
    order: Option<String>,
    cert: Option<String>,
    tokens: Tokens,
    http01: bool,
}

/// A response: status, Location, body (JSON when it parses).
struct Answer {
    status: u16,
    location: Option<String>,
    body: Value,
    text: String,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str| -> Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let scheme = s("scheme")?.unwrap_or_else(|| DEFAULT_SCHEME.to_owned());
    ensure!(
        scheme == "https" || scheme == "http",
        "scheme is https or http"
    );
    let path = s("directory_path")?.unwrap_or_else(|| DEFAULT_DIRECTORY_PATH.to_owned());
    ensure!(
        path.starts_with('/')
            && path.len() <= 256
            && !path.chars().any(|c| c.is_control() || c == ' '),
        "directory_path is an absolute path"
    );
    let directory = format!("{scheme}://{}{path}", ctx.remote_addr);
    let mut builder = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("netget-acme");
    if let Some(file) = s("ca_file")? {
        let pem = std::fs::read(&file).with_context(|| format!("reading ca_file {file}"))?;
        for cert in reqwest::Certificate::from_pem_bundle(&pem)
            .context("ca_file holds no PEM certificate")?
        {
            builder = builder.add_root_certificate(cert);
        }
    }
    let host = ctx
        .remote_addr
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(&ctx.remote_addr)
        .trim_matches(['[', ']']);
    if host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        builder = builder.no_proxy();
    }
    let http = builder.build()?;
    let resp = http
        .get(&directory)
        .send()
        .await
        .with_context(|| format!("fetching the directory {directory}"))?;
    ensure!(
        resp.status().is_success(),
        "the directory answered {}",
        resp.status()
    );
    let dir: Value =
        serde_json::from_slice(&read_limited(resp).await?).context("the directory is not JSON")?;
    for k in ["newNonce", "newAccount", "newOrder"] {
        ensure!(
            dir[k].as_str().is_some_and(|u| u.starts_with("http")),
            "the directory has no {k}"
        );
    }
    let tokens: Tokens = Arc::default();
    let mut local: SocketAddr = "0.0.0.0:0".parse()?;
    let http01 = match s("http01_listen")? {
        Some(addr) => {
            let listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("binding http01_listen {addr}"))?;
            local = listener.local_addr()?;
            let responder = tokio::spawn(responder(listener, tokens.clone()));
            ctx.state
                .register_client_task(ctx.client_id, responder)
                .await;
            true
        }
        None => false,
    };
    let session = Session {
        http,
        dir: dir.clone(),
        key: AccountKey::generate()?,
        kid: None,
        nonce: None,
        order: None,
        cert: None,
        tokens,
        http01,
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, json!({"directory": directory, "terms_of_service": dir["meta"]["termsOfService"], "external_account_required": dir["meta"]["externalAccountRequired"].as_bool().unwrap_or(false)})))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = AcmeClientProtocol;
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
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("ACME client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        run(&session_ctx, session, external, internal_rx, event_tx).await;
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

async fn read_limited(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        ensure!(
            out.len() + chunk.len() <= MAX_RESPONSE,
            "response over {} KiB",
            MAX_RESPONSE / 1024
        );
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// http-01 responder: GET /.well-known/acme-challenge/<token> answers the key authorization.
async fn responder(listener: tokio::net::TcpListener, tokens: Tokens) {
    use hyper::{server::conn::http1, service::service_fn, Response, StatusCode};
    let limiter = Arc::new(tokio::sync::Semaphore::new(64));
    while let Ok((stream, _)) = listener.accept().await {
        let Ok(permit) = limiter.clone().try_acquire_owned() else {
            continue;
        };
        let tokens = tokens.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let tokens = tokens.clone();
                async move {
                    let token = req
                        .uri()
                        .path()
                        .strip_prefix("/.well-known/acme-challenge/")
                        .unwrap_or_default()
                        .to_owned();
                    let answer = tokens.lock().await.get(&token).cloned();
                    let mut r =
                        Response::new(Full::new(Bytes::from(answer.clone().unwrap_or_default())));
                    *r.status_mut() = if answer.is_some() && req.method() == hyper::Method::GET {
                        StatusCode::OK
                    } else {
                        StatusCode::NOT_FOUND
                    };
                    Ok::<_, std::convert::Infallible>(r)
                }
            });
            let _ = tokio::time::timeout(
                TIMEOUT,
                http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service),
            )
            .await;
        });
    }
}

fn reply(command: Option<ClientCommand>, outcome: Result<ClientSendOutcome>) {
    if let Some(c) = command {
        crate::client::command_support::reply(c, outcome);
    }
}

async fn run(
    ctx: &ConnectContext,
    mut s: Session,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return },
            a = internal.recv() => match a { Some(a) => (a, None), None => return },
        };
        match AcmeClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                reply(command, Ok(ClientSendOutcome::Disconnected));
                return;
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
        let outcome = perform(&mut s, &action).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "ACME",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                reply(command, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                if events
                    .send(Event::new(&actions::RESPONSE_EVENT, data))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("ACME request failed: {e:#}"));
                reply(command, Err(anyhow::anyhow!("{e:#}")));
            }
        }
    }
}

impl Session {
    async fn nonce(&mut self) -> Result<String> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        let url = self.dir["newNonce"].as_str().unwrap_or_default().to_owned();
        let resp = self.http.head(&url).send().await?;
        resp.headers()
            .get("replay-nonce")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .context("newNonce gave no Replay-Nonce")
    }

    /// A signed POST (`payload: None` is POST-as-GET), retrying a badNonce twice (RFC 8555 §6.5).
    async fn post(&mut self, url: &str, payload: Option<&Value>, use_jwk: bool) -> Result<Answer> {
        for _ in 0..3 {
            let nonce = self.nonce().await?;
            let kid = if use_jwk {
                None
            } else {
                Some(
                    self.kid
                        .clone()
                        .context("no account yet: run acme_register first")?,
                )
            };
            let body = self.key.sign(url, &nonce, kid.as_deref(), payload)?;
            let resp = self
                .http
                .post(url)
                .header("content-type", "application/jose+json")
                .body(body.to_string())
                .send()
                .await?;
            self.nonce = resp
                .headers()
                .get("replay-nonce")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let status = resp.status().as_u16();
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let raw = read_limited(resp).await?;
            let text = String::from_utf8_lossy(&raw).into_owned();
            let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            if status == 400 && body["type"] == "urn:ietf:params:acme:error:badNonce" {
                continue;
            }
            return Ok(Answer {
                status,
                location,
                body,
                text,
            });
        }
        bail!("the CA rejected three nonces in a row")
    }
}

fn problem_of(a: &Answer) -> Value {
    json!({"type": a.body["type"], "detail": a.body["detail"]})
}

async fn perform(s: &mut Session, action: &Value) -> Result<Value> {
    match action["type"].as_str().unwrap_or_default() {
        "acme_register" => {
            let mut payload = json!({"termsOfServiceAgreed": action["agree_tos"]});
            if let Some(c) = action.get("contact").filter(|c| !c.is_null()) {
                payload["contact"] = c.clone();
            }
            let url = s.dir["newAccount"].as_str().unwrap_or_default().to_owned();
            let a = s.post(&url, Some(&payload), true).await?;
            if a.status < 300 {
                s.kid = Some(a.location.clone().context("newAccount gave no Location")?);
                return Ok(
                    json!({"operation": "register", "status": a.status, "account": s.kid, "account_status": a.body["status"]}),
                );
            }
            Ok(json!({"operation": "register", "status": a.status, "problem": problem_of(&a)}))
        }
        "acme_order" => {
            let ids: Vec<Value> = action["identifiers"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|i| json!({"type": "dns", "value": i}))
                .collect();
            let url = s.dir["newOrder"].as_str().unwrap_or_default().to_owned();
            let a = s
                .post(&url, Some(&json!({"identifiers": ids})), false)
                .await?;
            if a.status >= 300 {
                return Ok(
                    json!({"operation": "order", "status": a.status, "problem": problem_of(&a)}),
                );
            }
            s.order = Some(a.location.clone().context("newOrder gave no Location")?);
            s.cert = None;
            let authorizations = authorizations(s, &a.body).await?;
            Ok(
                json!({"operation": "order", "status": a.status, "order": s.order, "order_status": a.body["status"], "authorizations": authorizations}),
            )
        }
        "acme_validate" => validate(s, action).await,
        "acme_finalize" => finalize(s, action).await,
        "acme_revoke" => {
            let pem = s
                .cert
                .clone()
                .context("no certificate yet: run acme_finalize first")?;
            let der = rustls_pemfile::certs(&mut pem.as_bytes())
                .next()
                .context("the stored certificate does not parse")??;
            let mut payload = json!({"certificate": jws::b64(&der)});
            if let Some(r) = action.get("reason").filter(|r| !r.is_null()) {
                payload["reason"] = r.clone();
            }
            let url = s.dir["revokeCert"]
                .as_str()
                .context("the directory has no revokeCert")?
                .to_owned();
            let a = s.post(&url, Some(&payload), false).await?;
            let mut out = json!({"operation": "revoke", "status": a.status});
            if a.status >= 300 {
                out["problem"] = problem_of(&a);
            }
            Ok(out)
        }
        _ => {
            let kid = s
                .kid
                .clone()
                .context("no account yet: run acme_register first")?;
            let a = s
                .post(&kid, Some(&json!({"status": "deactivated"})), false)
                .await?;
            let mut out = json!({"operation": "deactivate", "status": a.status, "account_status": a.body["status"]});
            if a.status >= 300 {
                out["problem"] = problem_of(&a);
            }
            Ok(out)
        }
    }
}

/// POST-as-GET each authorization of `order`, with the dns-01 record each challenge needs.
async fn authorizations(s: &mut Session, order: &Value) -> Result<Vec<Value>> {
    let thumbprint = jws::thumbprint(&s.key.jwk())?;
    let mut out = Vec::new();
    for url in order["authorizations"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
    {
        let a = s.post(url, None, false).await?;
        ensure!(a.status == 200, "authorization {url} answered {}", a.status);
        let name = a.body["identifier"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let challenges: Vec<Value> = a.body["challenges"].as_array().cloned().unwrap_or_default().iter().map(|c| {
            let token = c["token"].as_str().unwrap_or_default();
            let mut v = json!({"type": c["type"], "status": c["status"], "token": token, "url": c["url"]});
            if c["type"] == "dns-01" {
                v["dns_txt_name"] = json!(format!("_acme-challenge.{name}"));
                v["dns_txt_value"] = json!(jws::b64(&jws::sha256(format!("{token}.{thumbprint}").as_bytes())));
            }
            v
        }).collect();
        out.push(json!({"url": url, "identifier": name, "status": a.body["status"], "wildcard": a.body["wildcard"].as_bool().unwrap_or(false), "challenges": challenges}));
    }
    Ok(out)
}

async fn current_order(s: &mut Session) -> Result<(String, Value)> {
    let url = s
        .order
        .clone()
        .context("no order yet: run acme_order first")?;
    let a = s.post(&url, None, false).await?;
    ensure!(a.status == 200, "the order answered {}", a.status);
    Ok((url, a.body))
}

async fn validate(s: &mut Session, action: &Value) -> Result<Value> {
    let (_, order) = current_order(s).await?;
    let wanted = action["identifier"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = action["challenge_type"].as_str().unwrap_or_default();
    let auths = authorizations(s, &order).await?;
    let auth = auths
        .iter()
        .find(|a| {
            let name = a["identifier"].as_str().unwrap_or_default();
            let full = if a["wildcard"] == true {
                format!("*.{name}")
            } else {
                name.to_owned()
            };
            full == wanted
        })
        .with_context(|| format!("the order has no authorization for {wanted}"))?;
    let challenge = auth["challenges"]
        .as_array()
        .and_then(|c| c.iter().find(|c| c["type"] == kind))
        .with_context(|| format!("the CA offers no {kind} for {wanted}"))?;
    if kind == "http-01" {
        ensure!(
            s.http01,
            "http-01 needs the http01_listen startup parameter"
        );
        let token = challenge["token"].as_str().unwrap_or_default().to_owned();
        let key_authorization = format!("{token}.{}", jws::thumbprint(&s.key.jwk())?);
        s.tokens.lock().await.insert(token, key_authorization);
    }
    let url = challenge["url"].as_str().unwrap_or_default().to_owned();
    let auth_url = auth["url"].as_str().unwrap_or_default().to_owned();
    let a = s.post(&url, Some(&json!({})), false).await?;
    if a.status >= 300 {
        return Ok(
            json!({"operation": "validate", "identifier": wanted, "status": a.status, "problem": problem_of(&a)}),
        );
    }
    for _ in 0..POLLS {
        let z = s.post(&auth_url, None, false).await?;
        let status = z.body["status"].as_str().unwrap_or_default().to_owned();
        if status != "pending" {
            let ch = z.body["challenges"]
                .as_array()
                .and_then(|c| c.iter().find(|c| c["type"] == kind))
                .cloned()
                .unwrap_or(Value::Null);
            let mut out = json!({"operation": "validate", "identifier": wanted, "status": z.status, "authorization_status": status, "challenge_status": ch["status"]});
            if !ch["error"].is_null() {
                out["problem"] =
                    json!({"type": ch["error"]["type"], "detail": ch["error"]["detail"]});
            }
            return Ok(out);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(
        json!({"operation": "validate", "identifier": wanted, "status": 200, "authorization_status": "pending", "challenge_status": "pending"}),
    )
}

async fn finalize(s: &mut Session, action: &Value) -> Result<Value> {
    let (order_url, order) = current_order(s).await?;
    let names: Vec<String> = order["identifiers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|i| i["value"].as_str().map(str::to_owned))
        .collect();
    ensure!(!names.is_empty(), "the order has no identifiers");
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(names.clone())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    let csr = params.serialize_request(&key)?;
    let finalize_url = order["finalize"]
        .as_str()
        .context("the order has no finalize URL")?
        .to_owned();
    let a = s
        .post(
            &finalize_url,
            Some(&json!({"csr": jws::b64(csr.der())})),
            false,
        )
        .await?;
    if a.status >= 300 {
        return Ok(json!({"operation": "finalize", "status": a.status, "problem": problem_of(&a)}));
    }
    let mut body = a.body;
    for _ in 0..POLLS {
        if body["status"] != "processing" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        body = s.post(&order_url, None, false).await?.body;
    }
    let mut out = json!({"operation": "finalize", "status": 200, "order_status": body["status"], "names": names});
    let Some(cert_url) = body["certificate"].as_str().map(str::to_owned) else {
        return Ok(out);
    };
    let c = s.post(&cert_url, None, false).await?;
    ensure!(
        c.status == 200 && c.text.contains("-----BEGIN CERTIFICATE-----"),
        "the certificate download answered {}",
        c.status
    );
    if let Some(path) = action["key_file"].as_str() {
        write_key(path, &key.serialize_pem())?;
        out["key_file"] = json!(path);
    }
    s.cert = Some(c.text.clone());
    out["certificate"] = json!(c.text);
    Ok(out)
}

fn write_key(path: &str, pem: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut f = options
        .open(path)
        .with_context(|| format!("creating key_file {path} (it must not exist)"))?;
    f.write_all(pem.as_bytes())?;
    Ok(())
}
