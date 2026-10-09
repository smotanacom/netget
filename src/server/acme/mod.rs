//! ACME (RFC 8555) certificate authority over HTTP/1.1, optionally over TLS. Rust owns the
//! directory, nonces, JWS verification, the account / order / authorization / challenge state
//! machine, http-01 fetching, CSR checks and issuance; the handler approves or refuses each
//! account, order, validation, issuance and revocation.
pub mod actions;
pub mod ca;
pub mod jws;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use jws::Jws;
use ring::rand::SecureRandom;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const DEFAULT_CA_NAME: &str = "NetGet ACME Test CA";
pub const DEFAULT_VALIDITY_DAYS: u32 = 90;
pub const DEFAULT_CHALLENGES: &[&str] = &["http-01", "dns-01"];
pub const MAX_BODY: usize = 64 * 1024;
const MAX_ACCOUNTS: usize = 1024;
const MAX_ORDERS: usize = 4096;
const MAX_CERTS: usize = 4096;
const MAX_NONCES: usize = 8192;
const MAX_IDENTIFIERS: usize = 100;
const ALGORITHMS: &[&str] = &["ES256", "ES384", "RS256", "EdDSA"];
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP01_TIMEOUT: Duration = Duration::from_secs(10);
const LIFETIME_DAYS: i64 = 7;

struct Account {
    jwk: Map<String, Value>,
    thumbprint: String,
    contact: Vec<String>,
    tos: bool,
    status: &'static str,
    orders: Vec<String>,
}

struct Order {
    account: String,
    identifiers: Vec<String>,
    authzs: Vec<String>,
    /// pending / ready / processing / valid / invalid, before authz roll-up.
    status: &'static str,
    cert: Option<String>,
    expires: String,
    error: Option<Value>,
}

struct Challenge {
    id: String,
    kind: String,
    token: String,
    status: &'static str,
    validated: Option<String>,
    error: Option<Value>,
}

struct Authz {
    account: String,
    identifier: String,
    status: &'static str,
    expires: String,
    challenges: Vec<Challenge>,
}

struct Cert {
    account: String,
    der: Vec<u8>,
    pem: String,
    serial: String,
    identifiers: Vec<String>,
    revoked: bool,
}

#[derive(Default)]
struct Store {
    nonces: HashSet<String>,
    nonce_order: VecDeque<String>,
    accounts: HashMap<String, Account>,
    by_thumbprint: HashMap<String, String>,
    orders: HashMap<String, Order>,
    authzs: HashMap<String, Authz>,
    challenge_authz: HashMap<String, String>,
    certs: HashMap<String, Cert>,
}

struct Shared {
    ctx: SpawnContext,
    ca: ca::Ca,
    validity_days: u32,
    challenge_types: Vec<String>,
    http01_target: Option<String>,
    terms_of_service: Option<String>,
    scheme: &'static str,
    store: Mutex<Store>,
}

pub async fn spawn(ctx: SpawnContext) -> anyhow::Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let s = |k: &str| -> anyhow::Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let ca_name = s("ca_name")?.unwrap_or_else(|| DEFAULT_CA_NAME.to_owned());
    anyhow::ensure!(
        !ca_name.is_empty()
            && ca_name.len() <= 64
            && !crate::utils::sanitize::has_controls(&ca_name),
        "ca_name is 1 to 64 printable characters"
    );
    let validity_days = p
        .map(|p| p.get_optional_u64("validity_days"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_VALIDITY_DAYS as u64);
    anyhow::ensure!(
        (1..=397).contains(&validity_days),
        "validity_days must be 1..=397"
    );
    let challenge_types: Vec<String> = match p
        .map(|p| p.get_optional_array("challenge_types"))
        .transpose()?
        .flatten()
    {
        None => DEFAULT_CHALLENGES.iter().map(|c| c.to_string()).collect(),
        Some(list) => list
            .iter()
            .map(|c| {
                c.as_str()
                    .filter(|c| matches!(*c, "http-01" | "dns-01"))
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("challenge_types holds http-01 and/or dns-01"))
            })
            .collect::<anyhow::Result<_>>()?,
    };
    anyhow::ensure!(
        !challenge_types.is_empty(),
        "challenge_types must offer at least one type"
    );
    let http01_target = s("http01_target")?;
    if let Some(t) = &http01_target {
        anyhow::ensure!(
            t.parse::<SocketAddr>().is_ok(),
            "http01_target is host:port with a literal address, e.g. 127.0.0.1:5002"
        );
    }
    let terms_of_service = s("terms_of_service")?;
    if let Some(t) = &terms_of_service {
        anyhow::ensure!(
            t.starts_with("https://") || t.starts_with("http://"),
            "terms_of_service is an http(s) URL"
        );
    }
    let tls = match (s("tls_cert_file")?, s("tls_key_file")?) {
        (Some(c), Some(k)) => Some(tokio_rustls::TlsAcceptor::from(
            crate::server::tls_cert_manager::load_tls_config_from_files(&c, &k)?,
        )),
        (None, None) => None,
        _ => anyhow::bail!("tls_cert_file and tls_key_file go together"),
    };
    let ca = ca::Ca::new(&ca_name)?;
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ACME directory at {scheme}://{local}/directory (CA \"{ca_name}\")"
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        ca,
        validity_days: validity_days as u32,
        challenge_types,
        http01_target,
        terms_of_service,
        scheme,
        store: Mutex::default(),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "ACME", Some(&shared.ctx.status_tx)).await {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
            shared
                .ctx
                .state
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
            let child = shared.clone();
            let tls = tls.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let svc_shared = child.clone();
                    let service = service_fn(move |req| {
                        let shared = svc_shared.clone();
                        async move { Ok::<_, Infallible>(handle(&shared, id, req).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(64)
                        .max_buf_size(32 * 1024);
                    let served = match tls {
                        Some(acceptor) => {
                            match tokio::time::timeout(TLS_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(s)) => builder
                                    .serve_connection(TokioIo::new(s), service)
                                    .await
                                    .map_err(|e| e.to_string()),
                                Ok(Err(e)) => Err(format!("TLS handshake: {e}")),
                                Err(_) => Err("TLS handshake timed out".into()),
                            }
                        }
                        None => builder
                            .serve_connection(TokioIo::new(stream), service)
                            .await
                            .map_err(|e| e.to_string()),
                    };
                    if let Err(e) = served {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("ACME connection {id}: {e}"));
                    }
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

type Reply = Response<Full<Bytes>>;

fn random_id() -> String {
    let mut b = [0u8; 16];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    jws::b64(&b)
}

fn rfc3339(offset_days: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::days(offset_days))
        .replace_nanosecond(0)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("ACME connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// An RFC 7807 problem with an ACME error type.
struct Problem {
    status: u16,
    kind: &'static str,
    detail: String,
    extra: Option<(&'static str, Value)>,
}

fn problem(status: u16, kind: &'static str, detail: impl Into<String>) -> Problem {
    Problem {
        status,
        kind,
        detail: detail.into(),
        extra: None,
    }
}

fn malformed(detail: impl Into<String>) -> Problem {
    problem(400, "malformed", detail)
}

impl Problem {
    fn body(&self) -> Value {
        let mut b = json!({"type": format!("urn:ietf:params:acme:error:{}", self.kind), "detail": self.detail, "status": self.status});
        if let Some((k, v)) = &self.extra {
            b[*k] = v.clone();
        }
        b
    }
}

/// What a successful route returns: status, body, content type, Location, extra Link.
struct Ok200 {
    status: u16,
    body: Bytes,
    content_type: &'static str,
    location: Option<String>,
    link: Option<String>,
}

fn json_reply(status: u16, body: &Value) -> Ok200 {
    Ok200 {
        status,
        body: Bytes::from(body.to_string()),
        content_type: "application/json",
        location: None,
        link: None,
    }
}

async fn handle(shared: &Arc<Shared>, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let ctx = &shared.ctx;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some(req.uri().to_string().len() as u64),
            None,
            Some(1),
            None,
        )
        .await;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty() && h.len() <= 255 && h.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned);
    let response = match host {
        None => finish(shared, "", Err(malformed("a Host header is required"))).await,
        Some(host) => {
            let base = format!("{}://{host}", shared.scheme);
            let result = route(shared, id, &base, req).await;
            finish(shared, &base, result).await
        }
    };
    let sent = hyper::body::Body::size_hint(response.body())
        .exact()
        .unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    response
}

async fn new_nonce(shared: &Shared) -> String {
    let nonce = random_id();
    let mut st = shared.store.lock().await;
    st.nonces.insert(nonce.clone());
    st.nonce_order.push_back(nonce.clone());
    while st.nonce_order.len() > MAX_NONCES {
        if let Some(old) = st.nonce_order.pop_front() {
            st.nonces.remove(&old);
        }
    }
    nonce
}

async fn finish(shared: &Shared, base: &str, result: Result<Ok200, Problem>) -> Reply {
    let nonce = new_nonce(shared).await;
    let (status, body, content_type, location, link) = match result {
        Ok(o) => (o.status, o.body, o.content_type, o.location, o.link),
        Err(p) => (
            p.status,
            Bytes::from(p.body().to_string()),
            "application/problem+json",
            None,
            None,
        ),
    };
    let has_body = !body.is_empty();
    let mut r = Response::new(Full::new(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let h = r.headers_mut();
    let mut set = |name: header::HeaderName, value: String| {
        if let Ok(v) = header::HeaderValue::from_str(&value) {
            h.append(name, v);
        }
    };
    if has_body {
        set(header::CONTENT_TYPE, content_type.to_owned());
    }
    set(header::HeaderName::from_static("replay-nonce"), nonce);
    set(header::CACHE_CONTROL, "no-store".into());
    if !base.is_empty() {
        set(header::LINK, format!("<{base}/directory>;rel=\"index\""));
    }
    if let Some(l) = link {
        set(header::LINK, l);
    }
    if let Some(l) = location {
        set(header::LOCATION, l);
    }
    r
}

async fn route(
    shared: &Arc<Shared>,
    id: ConnectionId,
    base: &str,
    req: Request<Incoming>,
) -> Result<Ok200, Problem> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.clone(), segments.as_slice()) {
        (Method::GET, ["directory"]) => Ok(json_reply(200, &directory(shared, base))),
        (Method::HEAD, ["new-nonce"]) => Ok(Ok200 {
            status: 200,
            body: Bytes::new(),
            content_type: "application/json",
            location: None,
            link: None,
        }),
        (Method::GET, ["new-nonce"]) => Ok(Ok200 {
            status: 204,
            body: Bytes::new(),
            content_type: "application/json",
            location: None,
            link: None,
        }),
        (Method::GET, ["roots", "0"]) => Ok(Ok200 {
            status: 200,
            body: Bytes::from(shared.ca.pem()),
            content_type: "application/pem-certificate-chain",
            location: None,
            link: None,
        }),
        (Method::POST, ["new-account" | "new-order" | "revoke-cert"])
        | (Method::POST, ["acct" | "order" | "authz" | "chall" | "cert", _])
        | (Method::POST, ["acct", _, "orders"])
        | (Method::POST, ["order", _, "finalize"]) => {
            let ct = req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ct.split(';').next().map(str::trim) != Some("application/jose+json") {
                return Err(problem(
                    415,
                    "malformed",
                    "POST bodies are application/jose+json",
                ));
            }
            let body = match tokio::time::timeout(
                BODY_TIMEOUT,
                Limited::new(req.into_body(), MAX_BODY).collect(),
            )
            .await
            {
                Ok(Ok(b)) => b.to_bytes(),
                Ok(Err(_)) => {
                    return Err(problem(
                        413,
                        "malformed",
                        format!("request body over {MAX_BODY} bytes"),
                    ))
                }
                Err(_) => return Err(malformed("request body timed out")),
            };
            let jws = Jws::parse(&body).map_err(|e| malformed(format!("{e:#}")))?;
            if !ALGORITHMS.contains(&jws.alg.as_str()) {
                let mut p = problem(
                    400,
                    "badSignatureAlgorithm",
                    format!("unsupported alg {}", jws.alg),
                );
                p.extra = Some(("algorithms", json!(ALGORITHMS)));
                return Err(p);
            }
            {
                let mut st = shared.store.lock().await;
                if !st.nonces.remove(&jws.nonce) {
                    return Err(problem(
                        400,
                        "badNonce",
                        "the nonce is unknown or was already used",
                    ));
                }
            }
            if jws.url != format!("{base}{path}") {
                return Err(problem(
                    403,
                    "unauthorized",
                    "the JWS url does not match the request URL",
                ));
            }
            post(shared, id, base, &segments, jws).await
        }
        (
            _,
            ["directory" | "new-nonce" | "roots" | "new-account" | "new-order" | "revoke-cert"
            | "acct" | "order" | "authz" | "chall" | "cert", ..],
        ) => Err(problem(
            405,
            "malformed",
            format!("{method} is not allowed here"),
        )),
        _ => Err(problem(404, "malformed", "no such resource")),
    }
}

fn directory(shared: &Shared, base: &str) -> Value {
    let mut meta = json!({"externalAccountRequired": false});
    if let Some(t) = &shared.terms_of_service {
        meta["termsOfService"] = json!(t);
    }
    json!({
        "newNonce": format!("{base}/new-nonce"),
        "newAccount": format!("{base}/new-account"),
        "newOrder": format!("{base}/new-order"),
        "revokeCert": format!("{base}/revoke-cert"),
        "meta": meta,
    })
}

fn key_type(jwk: &Map<String, Value>) -> String {
    jws::PublicKey::from_jwk(jwk)
        .map(|k| k.kind().to_owned())
        .unwrap_or_default()
}

/// Verify the JWS against its account (kid) and return the account id.
async fn authenticate(shared: &Shared, base: &str, jws: &Jws) -> Result<String, Problem> {
    let kid = jws.kid.as_deref().ok_or_else(|| {
        malformed("this request is signed with the account key, identified by kid")
    })?;
    let account = kid
        .strip_prefix(&format!("{base}/acct/"))
        .filter(|a| !a.contains('/'))
        .ok_or_else(|| {
            problem(
                400,
                "accountDoesNotExist",
                "kid is not an account URL of this server",
            )
        })?;
    let jwk = {
        let st = shared.store.lock().await;
        let a = st
            .accounts
            .get(account)
            .ok_or_else(|| problem(400, "accountDoesNotExist", "no such account"))?;
        if a.status != "valid" {
            return Err(problem(403, "unauthorized", "the account is deactivated"));
        }
        a.jwk.clone()
    };
    jws.verify(&jwk).map_err(|e| malformed(format!("{e:#}")))?;
    Ok(account.to_owned())
}

/// Ask the handler; `Ok(None)` is an acme_accept, `Ok(Some(problem))` its refusal.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Option<Problem>, Problem> {
    let ctx = &shared.ctx;
    let unavailable = |e: &anyhow::Error| {
        problem(
            500,
            "serverInternal",
            crate::utils::WireFailure::classify(e).text(),
        )
    };
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::AcmeProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err(unavailable(&e));
        }
    };
    let invalid = || {
        problem(
            500,
            "serverInternal",
            "the CA could not decide this request",
        )
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, operation, "fail_closed_invalid_reply");
        return Err(invalid());
    }
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    let answer = match answers.len() {
        1 => answers.remove(0),
        0 => {
            outcome(ctx, id, operation, "model_silent");
            return Err(invalid());
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            return Err(invalid());
        }
    };
    if answer["type"] == "acme_accept" {
        outcome(ctx, id, operation, "model_answer");
        return Ok(None);
    }
    outcome(ctx, id, operation, "model_reject");
    let kind = answer["error"].as_str().unwrap_or_default();
    let (kind, status) = actions::REJECT_ERRORS
        .iter()
        .find(|(n, _)| *n == kind)
        .copied()
        .unwrap_or(("unauthorized", 403));
    Ok(Some(problem(
        status,
        kind,
        answer["detail"].as_str().unwrap_or_default().to_owned(),
    )))
}

fn account_json(base: &str, id: &str, a: &Account) -> Value {
    let mut v = json!({"status": a.status, "contact": a.contact, "orders": format!("{base}/acct/{id}/orders")});
    if a.tos {
        v["termsOfServiceAgreed"] = json!(true);
    }
    v
}

fn check_contact(payload: &Value) -> Result<Vec<String>, Problem> {
    let Some(c) = payload.get("contact").filter(|c| !c.is_null()) else {
        return Ok(vec![]);
    };
    let list = c
        .as_array()
        .filter(|l| l.len() <= 10)
        .ok_or_else(|| malformed("contact is an array of at most 10 URLs"))?;
    list.iter()
        .map(|u| {
            let u = u
                .as_str()
                .ok_or_else(|| malformed("contact entries are strings"))?;
            let addr = u.strip_prefix("mailto:").ok_or_else(|| {
                problem(
                    400,
                    "unsupportedContact",
                    format!("only mailto: contacts are supported, not {u:?}"),
                )
            })?;
            let (local, domain) = addr.split_once('@').unwrap_or_default();
            if local.is_empty()
                || domain.is_empty()
                || u.len() > 256
                || addr.contains(['?', ',', ' '])
                || crate::utils::sanitize::has_controls(&addr)
            {
                return Err(problem(
                    400,
                    "invalidContact",
                    format!("{u:?} is not a single email address"),
                ));
            }
            Ok(u.to_owned())
        })
        .collect()
}

/// A DNS identifier as RFC 8555 §7.1.4 and the CA/B rules allow it: lower-cased LDH labels,
/// at most one leading wildcard label.
fn dns_name(v: &str) -> Option<String> {
    let v = v.to_ascii_lowercase();
    let rest = v.strip_prefix("*.").unwrap_or(&v);
    let ok = !rest.is_empty()
        && v.len() <= 253
        && rest.contains('.')
        && rest.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && rest.parse::<std::net::IpAddr>().is_err();
    ok.then_some(v)
}

/// Roll authorization outcomes up into the order (RFC 8555 §7.1.6).
fn order_status(st: &Store, o: &Order) -> &'static str {
    match o.status {
        "pending" => {
            let states: Vec<&str> = o
                .authzs
                .iter()
                .filter_map(|a| st.authzs.get(a))
                .map(|a| a.status)
                .collect();
            if states.iter().any(|s| *s != "pending" && *s != "valid") {
                "invalid"
            } else if !states.is_empty() && states.iter().all(|s| *s == "valid") {
                "ready"
            } else {
                "pending"
            }
        }
        s => s,
    }
}

fn order_json(st: &Store, base: &str, id: &str, o: &Order) -> Value {
    let mut v = json!({
        "status": order_status(st, o),
        "expires": o.expires,
        "identifiers": o.identifiers.iter().map(|i| json!({"type": "dns", "value": i})).collect::<Vec<_>>(),
        "authorizations": o.authzs.iter().map(|a| format!("{base}/authz/{a}")).collect::<Vec<_>>(),
        "finalize": format!("{base}/order/{id}/finalize"),
    });
    if let Some(c) = &o.cert {
        v["certificate"] = json!(format!("{base}/cert/{c}"));
    }
    if let Some(e) = &o.error {
        v["error"] = e.clone();
    }
    v
}

fn challenge_json(base: &str, c: &Challenge) -> Value {
    let mut v = json!({"type": c.kind, "url": format!("{base}/chall/{}", c.id), "status": c.status, "token": c.token});
    if let Some(t) = &c.validated {
        v["validated"] = json!(t);
    }
    if let Some(e) = &c.error {
        v["error"] = e.clone();
    }
    v
}

fn authz_json(base: &str, a: &Authz) -> Value {
    let wildcard = a.identifier.starts_with("*.");
    let mut v = json!({
        "status": a.status,
        "expires": a.expires,
        "identifier": {"type": "dns", "value": a.identifier.trim_start_matches("*.")},
        "challenges": a.challenges.iter().map(|c| challenge_json(base, c)).collect::<Vec<_>>(),
    });
    if wildcard {
        v["wildcard"] = json!(true);
    }
    v
}

fn owned<'a, T>(
    item: Option<&'a T>,
    owner: impl Fn(&T) -> &str,
    account: &str,
) -> Result<&'a T, Problem> {
    let item = item.ok_or_else(|| problem(404, "malformed", "no such resource"))?;
    if owner(item) != account {
        return Err(problem(
            403,
            "unauthorized",
            "this resource belongs to another account",
        ));
    }
    Ok(item)
}

async fn post(
    shared: &Arc<Shared>,
    id: ConnectionId,
    base: &str,
    segments: &[&str],
    jws: Jws,
) -> Result<Ok200, Problem> {
    match segments {
        ["new-account"] => new_account(shared, id, base, jws).await,
        ["revoke-cert"] => {
            let account = authenticate(shared, base, &jws).await?;
            revoke(shared, id, base, &account, &jws).await
        }
        ["new-order"] => {
            let account = authenticate(shared, base, &jws).await?;
            new_order(shared, id, base, &account, &jws).await
        }
        ["acct", acct] => {
            let account = authenticate(shared, base, &jws).await?;
            if account != *acct {
                return Err(problem(
                    403,
                    "unauthorized",
                    "the kid names another account",
                ));
            }
            let mut st = shared.store.lock().await;
            let a = st
                .accounts
                .get_mut(*acct)
                .ok_or_else(|| problem(400, "accountDoesNotExist", "no such account"))?;
            if !jws.payload.is_empty() {
                let payload = jws
                    .payload_json()
                    .map_err(|e| malformed(format!("{e:#}")))?;
                if payload.get("contact").is_some_and(|c| !c.is_null()) {
                    a.contact = check_contact(&payload)?;
                }
                match payload.get("status").and_then(Value::as_str) {
                    Some("deactivated") => a.status = "deactivated",
                    Some(other) => {
                        return Err(malformed(format!("an account cannot be set to {other}")))
                    }
                    None => {}
                }
            }
            Ok(json_reply(200, &account_json(base, acct, a)))
        }
        ["acct", acct, "orders"] => {
            let account = authenticate(shared, base, &jws).await?;
            if account != *acct {
                return Err(problem(
                    403,
                    "unauthorized",
                    "the kid names another account",
                ));
            }
            let st = shared.store.lock().await;
            let orders: Vec<String> = st
                .accounts
                .get(*acct)
                .map(|a| {
                    a.orders
                        .iter()
                        .map(|o| format!("{base}/order/{o}"))
                        .collect()
                })
                .unwrap_or_default();
            Ok(json_reply(200, &json!({"orders": orders})))
        }
        ["order", order] => {
            let account = authenticate(shared, base, &jws).await?;
            let st = shared.store.lock().await;
            let o = owned(st.orders.get(*order), |o: &Order| &o.account, &account)?;
            Ok(json_reply(200, &order_json(&st, base, order, o)))
        }
        ["order", order, "finalize"] => {
            let account = authenticate(shared, base, &jws).await?;
            finalize(shared, id, base, &account, order, &jws).await
        }
        ["authz", authz] => {
            let account = authenticate(shared, base, &jws).await?;
            let mut st = shared.store.lock().await;
            owned(st.authzs.get(*authz), |a: &Authz| &a.account, &account)?;
            if !jws.payload.is_empty() {
                let payload = jws
                    .payload_json()
                    .map_err(|e| malformed(format!("{e:#}")))?;
                if payload.get("status").and_then(Value::as_str) != Some("deactivated") {
                    return Err(malformed("an authorization can only be deactivated"));
                }
                if let Some(a) = st.authzs.get_mut(*authz) {
                    if a.status == "pending" || a.status == "valid" {
                        a.status = "deactivated";
                    }
                }
            }
            let a = st
                .authzs
                .get(*authz)
                .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
            Ok(json_reply(200, &authz_json(base, a)))
        }
        ["chall", chall] => {
            let account = authenticate(shared, base, &jws).await?;
            challenge(shared, id, base, &account, chall, &jws).await
        }
        ["cert", cert] => {
            let account = authenticate(shared, base, &jws).await?;
            if !jws.payload.is_empty() {
                return Err(malformed("certificates are fetched with POST-as-GET"));
            }
            let st = shared.store.lock().await;
            let c = owned(st.certs.get(*cert), |c: &Cert| &c.account, &account)?;
            Ok(Ok200 {
                status: 200,
                body: Bytes::from(c.pem.clone()),
                content_type: "application/pem-certificate-chain",
                location: None,
                link: None,
            })
        }
        _ => Err(problem(404, "malformed", "no such resource")),
    }
}

async fn new_account(
    shared: &Shared,
    id: ConnectionId,
    base: &str,
    jws: Jws,
) -> Result<Ok200, Problem> {
    let jwk = jws
        .jwk
        .clone()
        .ok_or_else(|| malformed("newAccount is signed with the new key, embedded as jwk"))?;
    jws.verify(&jwk).map_err(|e| malformed(format!("{e:#}")))?;
    let thumbprint = jws::thumbprint(&jwk).map_err(|e| malformed(format!("{e:#}")))?;
    let payload = jws
        .payload_json()
        .map_err(|e| malformed(format!("{e:#}")))?;
    {
        let st = shared.store.lock().await;
        if let Some(existing) = st.by_thumbprint.get(&thumbprint) {
            let a = &st.accounts[existing];
            let mut r = json_reply(200, &account_json(base, existing, a));
            r.location = Some(format!("{base}/acct/{existing}"));
            return Ok(r);
        }
    }
    if payload.get("onlyReturnExisting").and_then(Value::as_bool) == Some(true) {
        return Err(problem(
            400,
            "accountDoesNotExist",
            "no account exists for this key",
        ));
    }
    let contact = check_contact(&payload)?;
    let tos = payload.get("termsOfServiceAgreed").and_then(Value::as_bool) == Some(true);
    if shared.terms_of_service.is_some() && !tos {
        return Err(malformed("the terms of service must be agreed to"));
    }
    let event = Event::new(
        &actions::NEW_ACCOUNT_EVENT,
        json!({"contact": contact, "terms_of_service_agreed": tos, "key_type": key_type(&jwk), "thumbprint": thumbprint}),
    );
    if let Some(refusal) = ask(shared, id, event, "new-account").await? {
        return Err(refusal);
    }
    let mut st = shared.store.lock().await;
    if let Some(existing) = st.by_thumbprint.get(&thumbprint).cloned() {
        let mut r = json_reply(200, &account_json(base, &existing, &st.accounts[&existing]));
        r.location = Some(format!("{base}/acct/{existing}"));
        return Ok(r);
    }
    if st.accounts.len() >= MAX_ACCOUNTS {
        return Err(problem(
            429,
            "rateLimited",
            "this CA holds its maximum number of accounts",
        ));
    }
    let acct = random_id();
    let a = Account {
        jwk,
        thumbprint: thumbprint.clone(),
        contact,
        tos,
        status: "valid",
        orders: vec![],
    };
    let body = account_json(base, &acct, &a);
    st.accounts.insert(acct.clone(), a);
    st.by_thumbprint.insert(thumbprint, acct.clone());
    let mut r = json_reply(201, &body);
    r.location = Some(format!("{base}/acct/{acct}"));
    Ok(r)
}

async fn new_order(
    shared: &Shared,
    id: ConnectionId,
    base: &str,
    account: &str,
    jws: &Jws,
) -> Result<Ok200, Problem> {
    let payload = jws
        .payload_json()
        .map_err(|e| malformed(format!("{e:#}")))?;
    if payload.get("notBefore").is_some_and(|v| !v.is_null())
        || payload.get("notAfter").is_some_and(|v| !v.is_null())
    {
        return Err(malformed(
            "notBefore and notAfter are not supported; validity is set by the CA",
        ));
    }
    let list = payload
        .get("identifiers")
        .and_then(Value::as_array)
        .filter(|l| !l.is_empty() && l.len() <= MAX_IDENTIFIERS)
        .ok_or_else(|| {
            malformed(format!(
                "identifiers is an array of 1 to {MAX_IDENTIFIERS} objects"
            ))
        })?;
    let mut identifiers: Vec<String> = Vec::new();
    for i in list {
        if i.get("type").and_then(Value::as_str) != Some("dns") {
            return Err(problem(
                400,
                "unsupportedIdentifier",
                "only dns identifiers are supported",
            ));
        }
        let raw = i.get("value").and_then(Value::as_str).unwrap_or_default();
        let name = dns_name(raw).ok_or_else(|| {
            problem(
                400,
                "rejectedIdentifier",
                format!("{raw:?} is not a valid DNS name"),
            )
        })?;
        if name.starts_with("*.") && !shared.challenge_types.iter().any(|c| c == "dns-01") {
            return Err(problem(
                400,
                "rejectedIdentifier",
                "wildcards need dns-01, which this CA does not offer",
            ));
        }
        if !identifiers.contains(&name) {
            identifiers.push(name);
        }
    }
    let contact = shared
        .store
        .lock()
        .await
        .accounts
        .get(account)
        .map(|a| a.contact.clone())
        .unwrap_or_default();
    let event = Event::new(
        &actions::NEW_ORDER_EVENT,
        json!({"account": format!("{base}/acct/{account}"), "contact": contact, "identifiers": identifiers}),
    );
    if let Some(refusal) = ask(shared, id, event, "new-order").await? {
        return Err(refusal);
    }
    let mut st = shared.store.lock().await;
    if st.orders.len() >= MAX_ORDERS {
        return Err(problem(
            429,
            "rateLimited",
            "this CA holds its maximum number of orders",
        ));
    }
    let expires = rfc3339(LIFETIME_DAYS);
    let order = random_id();
    let mut authzs = Vec::new();
    for name in &identifiers {
        let authz = random_id();
        let kinds: Vec<&String> = shared
            .challenge_types
            .iter()
            .filter(|c| !name.starts_with("*.") || *c == "dns-01")
            .collect();
        let challenges: Vec<Challenge> = kinds
            .into_iter()
            .map(|k| Challenge {
                id: random_id(),
                kind: k.clone(),
                token: random_id(),
                status: "pending",
                validated: None,
                error: None,
            })
            .collect();
        for c in &challenges {
            st.challenge_authz.insert(c.id.clone(), authz.clone());
        }
        st.authzs.insert(
            authz.clone(),
            Authz {
                account: account.to_owned(),
                identifier: name.clone(),
                status: "pending",
                expires: expires.clone(),
                challenges,
            },
        );
        authzs.push(authz);
    }
    let o = Order {
        account: account.to_owned(),
        identifiers,
        authzs,
        status: "pending",
        cert: None,
        expires,
        error: None,
    };
    let body = order_json(&st, base, &order, &o);
    st.orders.insert(order.clone(), o);
    if let Some(a) = st.accounts.get_mut(account) {
        a.orders.push(order.clone());
    }
    let mut r = json_reply(201, &body);
    r.location = Some(format!("{base}/order/{order}"));
    Ok(r)
}

/// http-01 (RFC 8555 §8.3): GET the token from `target` with the identifier as Host.
async fn fetch_http01(target: &str, identifier: &str, token: &str) -> Result<String, String> {
    let fetch = async {
        let stream = tokio::net::TcpStream::connect(target)
            .await
            .map_err(|e| format!("connection refused: {e}"))?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake::<_, Full<Bytes>>(TokioIo::new(stream))
                .await
                .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let req = Request::get(format!("/.well-known/acme-challenge/{token}"))
            .header(header::HOST, identifier)
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())?;
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status();
        let body = Limited::new(resp.into_body(), 1024)
            .collect()
            .await
            .map_err(|_| "response body over 1 KiB".to_owned())?
            .to_bytes();
        if status != StatusCode::OK {
            return Err(format!("the response status was {status}"));
        }
        Ok(String::from_utf8_lossy(&body).trim_end().to_owned())
    };
    tokio::time::timeout(HTTP01_TIMEOUT, fetch)
        .await
        .map_err(|_| "the fetch timed out".to_owned())?
}

async fn challenge(
    shared: &Shared,
    id: ConnectionId,
    base: &str,
    account: &str,
    chall: &str,
    jws: &Jws,
) -> Result<Ok200, Problem> {
    let (authz, kind, token, identifier, status, thumbprint) = {
        let st = shared.store.lock().await;
        let authz = st
            .challenge_authz
            .get(chall)
            .cloned()
            .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
        let a = owned(st.authzs.get(&authz), |a: &Authz| &a.account, account)?;
        let c = a
            .challenges
            .iter()
            .find(|c| c.id == chall)
            .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
        let thumbprint = st
            .accounts
            .get(account)
            .map(|a| a.thumbprint.clone())
            .unwrap_or_default();
        (
            authz.clone(),
            c.kind.clone(),
            c.token.clone(),
            a.identifier.trim_start_matches("*.").to_owned(),
            (c.status, a.status),
            thumbprint,
        )
    };
    let respond = |st: &Store| -> Result<Ok200, Problem> {
        let a = st
            .authzs
            .get(&authz)
            .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
        let c = a
            .challenges
            .iter()
            .find(|c| c.id == chall)
            .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
        let mut r = json_reply(200, &challenge_json(base, c));
        r.link = Some(format!("<{base}/authz/{authz}>;rel=\"up\""));
        Ok(r)
    };
    // An empty payload is POST-as-GET; "{}" asks for validation (RFC 8555 §7.5.1).
    if jws.payload.is_empty() || status != ("pending", "pending") {
        return respond(&*shared.store.lock().await);
    }
    if !jws
        .payload_json()
        .map_err(|e| malformed(format!("{e:#}")))?
        .is_object()
    {
        return Err(malformed("a challenge response is a JSON object"));
    }
    let key_authorization = format!("{token}.{thumbprint}");
    let mut verified = Value::Null;
    if kind == "http-01" {
        if let Some(target) = &shared.http01_target {
            match fetch_http01(target, &identifier, &token).await {
                Ok(body) if body == key_authorization => verified = json!(true),
                result => {
                    let detail = match result {
                        Ok(body) => format!(
                            "the key authorization at {identifier} was {:?}",
                            crate::utils::truncate::truncate_for_log(&body, 64)
                        ),
                        Err(e) => format!(
                            "fetching http://{identifier}/.well-known/acme-challenge/{token}: {e}"
                        ),
                    };
                    outcome(&shared.ctx, id, "validate", "protocol_refusal");
                    let mut st = shared.store.lock().await;
                    fail_challenge(
                        &mut st,
                        &authz,
                        chall,
                        problem(403, "incorrectResponse", detail).body(),
                    );
                    return respond(&st);
                }
            }
        }
    }
    let event = Event::new(
        &actions::VALIDATE_EVENT,
        json!({"account": format!("{base}/acct/{account}"), "identifier": identifier, "challenge_type": kind, "token": token, "key_authorization": key_authorization, "verified": verified}),
    );
    let decision = ask(shared, id, event, "validate").await?;
    let mut st = shared.store.lock().await;
    match decision {
        None => {
            if let Some(a) = st.authzs.get_mut(&authz) {
                if a.status == "pending" {
                    a.status = "valid";
                    if let Some(c) = a.challenges.iter_mut().find(|c| c.id == chall) {
                        c.status = "valid";
                        c.validated = Some(rfc3339(0));
                    }
                }
            }
        }
        Some(refusal) => fail_challenge(&mut st, &authz, chall, refusal.body()),
    }
    respond(&st)
}

fn fail_challenge(st: &mut Store, authz: &str, chall: &str, error: Value) {
    if let Some(a) = st.authzs.get_mut(authz) {
        a.status = "invalid";
        if let Some(c) = a.challenges.iter_mut().find(|c| c.id == chall) {
            c.status = "invalid";
            c.error = Some(error);
        }
    }
}

async fn finalize(
    shared: &Shared,
    id: ConnectionId,
    base: &str,
    account: &str,
    order: &str,
    jws: &Jws,
) -> Result<Ok200, Problem> {
    let identifiers = {
        let st = shared.store.lock().await;
        let o = owned(st.orders.get(order), |o: &Order| &o.account, account)?;
        let status = order_status(&st, o);
        if status != "ready" {
            return Err(problem(
                403,
                "orderNotReady",
                format!("the order is {status}, not ready"),
            ));
        }
        o.identifiers.clone()
    };
    let payload = jws
        .payload_json()
        .map_err(|e| malformed(format!("{e:#}")))?;
    let der = payload
        .get("csr")
        .and_then(Value::as_str)
        .ok_or_else(|| malformed("csr is required"))
        .and_then(|c| jws::unb64(c).map_err(|_| problem(400, "badCSR", "csr is not base64url")))?;
    let csr = ca::Ca::read_csr(&der).map_err(|e| problem(400, "badCSR", format!("{e:#}")))?;
    let mut asked: Vec<String> = csr.dns_names.clone();
    asked.sort();
    asked.dedup();
    let mut wanted = identifiers.clone();
    wanted.sort();
    if csr.other_names > 0 || asked != wanted {
        return Err(problem(
            400,
            "badCSR",
            format!("the CSR names {asked:?}; the order is for {wanted:?}"),
        ));
    }
    if let Some(cn) = &csr.common_name {
        if !wanted.contains(&cn.to_ascii_lowercase()) {
            return Err(problem(
                400,
                "badCSR",
                format!("the CSR common name {cn:?} is not one of the order's identifiers"),
            ));
        }
    }
    let event = Event::new(
        &actions::FINALIZE_EVENT,
        json!({"account": format!("{base}/acct/{account}"), "order": format!("{base}/order/{order}"), "identifiers": identifiers, "validity_days": shared.validity_days}),
    );
    let decision = ask(shared, id, event, "finalize").await?;
    let mut st = shared.store.lock().await;
    if let Some(refusal) = decision {
        if let Some(o) = st.orders.get_mut(order) {
            o.status = "invalid";
            o.error = Some(refusal.body());
        }
        return Err(refusal);
    }
    if st.certs.len() >= MAX_CERTS {
        return Err(problem(
            429,
            "rateLimited",
            "this CA holds its maximum number of certificates",
        ));
    }
    let issued = shared
        .ca
        .issue(csr, &identifiers, shared.validity_days)
        .map_err(|e| problem(500, "serverInternal", format!("issuance failed: {e:#}")))?;
    let cert = random_id();
    st.certs.insert(
        cert.clone(),
        Cert {
            account: account.to_owned(),
            der: issued.der,
            pem: issued.pem,
            serial: issued.serial,
            identifiers: identifiers.clone(),
            revoked: false,
        },
    );
    let o = st
        .orders
        .get_mut(order)
        .ok_or_else(|| problem(404, "malformed", "no such resource"))?;
    o.status = "valid";
    o.cert = Some(cert);
    let o = &st.orders[order];
    let mut r = json_reply(200, &order_json(&st, base, order, o));
    r.location = Some(format!("{base}/order/{order}"));
    Ok(r)
}

async fn revoke(
    shared: &Shared,
    id: ConnectionId,
    base: &str,
    account: &str,
    jws: &Jws,
) -> Result<Ok200, Problem> {
    let payload = jws
        .payload_json()
        .map_err(|e| malformed(format!("{e:#}")))?;
    let der = payload
        .get("certificate")
        .and_then(Value::as_str)
        .and_then(|c| jws::unb64(c).ok())
        .ok_or_else(|| malformed("certificate is base64url DER"))?;
    let reason = match payload.get("reason").filter(|r| !r.is_null()) {
        None => None,
        Some(r) => match r.as_u64() {
            Some(n @ (0..=6 | 8..=10)) => Some(n),
            _ => {
                return Err(problem(
                    400,
                    "badRevocationReason",
                    "reason is an RFC 5280 code 0 to 10, not 7",
                ))
            }
        },
    };
    let (cert, serial, identifiers) = {
        let st = shared.store.lock().await;
        let (cert, c) =
            st.certs.iter().find(|(_, c)| c.der == der).ok_or_else(|| {
                problem(404, "malformed", "this CA did not issue that certificate")
            })?;
        if c.account != account {
            return Err(problem(
                403,
                "unauthorized",
                "the certificate belongs to another account",
            ));
        }
        if c.revoked {
            return Err(problem(
                400,
                "alreadyRevoked",
                "the certificate is already revoked",
            ));
        }
        (cert.clone(), c.serial.clone(), c.identifiers.clone())
    };
    let event = Event::new(
        &actions::REVOKE_EVENT,
        json!({"account": format!("{base}/acct/{account}"), "serial": serial, "identifiers": identifiers, "reason": reason}),
    );
    if let Some(refusal) = ask(shared, id, event, "revoke").await? {
        return Err(refusal);
    }
    if let Some(c) = shared.store.lock().await.certs.get_mut(&cert) {
        c.revoked = true;
    }
    Ok(Ok200 {
        status: 200,
        body: Bytes::new(),
        content_type: "application/json",
        location: None,
        link: None,
    })
}
