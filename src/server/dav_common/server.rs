//! The DAV server engine CalDAV and CardDAV share. Rust owns the URL space (principals, homes,
//! collections, objects), discovery, PROPFIND/REPORT evaluation, object validation, ETags,
//! preconditions and authentication; the handler owns logins and every collection and object.
use super::object::{self, Component};
use super::xml::{self, PropName, PropRequest, Report, CALDAV, CARDDAV, CS, DAV};
use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::{ActionResult, Server};
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use base64::Engine;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::Incoming, header, server::conn::http1, service::service_fn, Method, Request, Response,
    StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::LazyLock;
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const DEFAULT_AUTH: &str = "required";
pub const DEFAULT_USER: &str = "user";
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_CACHE: Duration = Duration::from_secs(1800);
const MAX_COLLECTIONS: usize = 256;
const MAX_OBJECTS: usize = 5000;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Calendar,
    AddressBook,
}

/// What distinguishes CalDAV from CardDAV for the engine.
pub struct Flavor {
    pub kind: Kind,
    pub name: &'static str,
    /// Event and action prefix: `caldav` or `carddav`.
    pub prefix: &'static str,
    pub root: &'static str,
    pub well_known: &'static str,
    pub extension: &'static str,
    pub login_event: &'static LazyLock<EventType>,
    pub request_event: &'static LazyLock<EventType>,
    pub protocol: &'static (dyn Server + Sync),
}

impl Flavor {
    fn ns(&self) -> &'static str {
        match self.kind {
            Kind::Calendar => CALDAV,
            Kind::AddressBook => CARDDAV,
        }
    }
    fn action(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.prefix)
    }
    fn content_type(&self, component: &str) -> String {
        match self.kind {
            Kind::Calendar => format!(
                "text/calendar; charset=utf-8; component={}",
                component.to_ascii_lowercase()
            ),
            Kind::AddressBook => "text/vcard; charset=utf-8".into(),
        }
    }
}

struct Shared {
    ctx: SpawnContext,
    flavor: &'static Flavor,
    auth_required: bool,
    default_user: String,
    logins: Mutex<HashMap<u64, Instant>>,
    login_key: std::collections::hash_map::RandomState,
}

pub fn read_auth(ctx: &SpawnContext) -> anyhow::Result<(bool, String)> {
    let p = ctx.startup_params.as_ref();
    let auth = p
        .map(|p| p.get_optional_string("auth"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_AUTH.to_owned());
    anyhow::ensure!(
        auth == "required" || auth == "none",
        "auth must be required or none"
    );
    let user = p
        .map(|p| p.get_optional_string("default_user"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_USER.to_owned());
    anyhow::ensure!(segment_ok(&user), "default_user must be a simple name");
    Ok((auth == "required", user))
}

pub async fn spawn(ctx: SpawnContext, flavor: &'static Flavor) -> anyhow::Result<SocketAddr> {
    let (auth_required, default_user) = read_auth(&ctx)?;
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "{} server at http://{local}/ (well-known {})",
        flavor.name, flavor.well_known
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        flavor,
        auth_required,
        default_user,
        logins: Mutex::default(),
        login_key: Default::default(),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(&listener, &limiter, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", flavor.name, Some(&shared.ctx.status_tx)).await {
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
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let svc = child.clone();
                    let service = service_fn(move |req| {
                        let shared = svc.clone();
                        async move { Ok::<_, Infallible>(handle(&shared, id, req).await) }
                    });
                    let mut builder = http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_TIMEOUT)
                        .max_headers(64)
                        .max_buf_size(64 * 1024);
                    if let Err(e) = builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("{} connection {id}: {e}", child.flavor.name));
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

fn reply(
    status: u16,
    content_type: Option<&str>,
    body: Vec<u8>,
    headers: &[(&'static str, String)],
) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Some(ct) = content_type.and_then(|c| header::HeaderValue::from_str(c).ok()) {
        r.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    for (k, v) in headers {
        if let Ok(v) = header::HeaderValue::from_str(v) {
            r.headers_mut().insert(*k, v);
        }
    }
    r
}

fn plain(status: u16, text: &str) -> Reply {
    reply(
        status,
        Some("text/plain; charset=utf-8"),
        text.as_bytes().to_vec(),
        &[],
    )
}

fn multistatus(responses: &[String]) -> Reply {
    reply(
        207,
        Some("application/xml; charset=utf-8"),
        xml::multistatus(responses).into_bytes(),
        &[],
    )
}

fn precondition(status: u16, ns: &str, condition: &str) -> Reply {
    reply(
        status,
        Some("application/xml; charset=utf-8"),
        xml::error_body(ns, condition).into_bytes(),
        &[],
    )
}

fn outcome(ctx: &SpawnContext, flavor: &Flavor, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!(
        "{} connection {id} operation={operation} decision={decision}",
        flavor.name
    );
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

pub fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-@+~%".contains(&b))
}

/// The ETag Rust gives an object whose handler supplied none: a hash of its bytes.
pub fn etag_of(data: &str) -> String {
    let mut h = std::hash::DefaultHasher::new();
    h.write(data.as_bytes());
    format!("\"h{:016x}\"", h.finish())
}

fn quote_etag(e: &str) -> String {
    if e.starts_with('"') || e.starts_with("W/\"") {
        e.to_owned()
    } else {
        format!("\"{e}\"")
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Target {
    Root,
    Principals,
    Principal(String),
    Home(String),
    Collection(String, String),
    Object(String, String, String),
}

fn target(flavor: &Flavor, path: &str) -> Option<Target> {
    let decoded = urlencoding::decode(path).ok()?;
    let segs: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    if !segs.iter().all(|s| segment_ok(s)) {
        return None;
    }
    Some(match segs.as_slice() {
        [] => Target::Root,
        ["principals"] => Target::Principals,
        ["principals", u] => Target::Principal(u.to_string()),
        [r, u] if *r == flavor.root => Target::Home(u.to_string()),
        [r, u, c] if *r == flavor.root => Target::Collection(u.to_string(), c.to_string()),
        [r, u, c, o] if *r == flavor.root => {
            Target::Object(u.to_string(), c.to_string(), o.to_string())
        }
        _ => return None,
    })
}

fn href(flavor: &Flavor, t: &Target) -> String {
    match t {
        Target::Root => "/".into(),
        Target::Principals => "/principals/".into(),
        Target::Principal(u) => format!("/principals/{u}/"),
        Target::Home(u) => format!("/{}/{u}/", flavor.root),
        Target::Collection(u, c) => format!("/{}/{u}/{c}/", flavor.root),
        Target::Object(u, c, o) => format!("/{}/{u}/{c}/{o}", flavor.root),
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
    let r = route(shared, id, req).await;
    let sent = hyper::body::Body::size_hint(r.body()).exact().unwrap_or(0);
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(sent), None, Some(1))
        .await;
    r
}

/// The user the request is for: Basic auth decided by the handler (and remembered as a keyed
/// hash), or the configured default user when authentication is off.
async fn authenticate(
    shared: &Shared,
    id: ConnectionId,
    req: &Request<Incoming>,
) -> Result<String, Reply> {
    if !shared.auth_required {
        return Ok(shared.default_user.clone());
    }
    let unauthorized = || {
        let mut r = plain(401, "authentication required");
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Basic realm=\"NetGet DAV\", charset=\"UTF-8\""),
        );
        r
    };
    let creds = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| {
            base64::engine::general_purpose::STANDARD
                .decode(b.trim())
                .ok()
        })
        .and_then(|b| String::from_utf8(b).ok());
    let Some((user, password)) = creds.as_deref().and_then(|c| c.split_once(':')) else {
        return Err(unauthorized());
    };
    if !segment_ok(user) {
        return Err(unauthorized());
    }
    let key = {
        let mut h = shared.login_key.build_hasher();
        h.write(user.as_bytes());
        h.write_u8(0);
        h.write(password.as_bytes());
        h.finish()
    };
    {
        let mut cache = shared.logins.lock().await;
        cache.retain(|_, t| t.elapsed() < LOGIN_CACHE);
        if cache.contains_key(&key) {
            return Ok(user.to_owned());
        }
    }
    let flavor = shared.flavor;
    let event = Event::new(
        flavor.login_event,
        json!({"user_name": user, "password": password}),
    );
    let answers = ask(
        shared,
        id,
        event,
        "login",
        &[
            &flavor.action("login_accept"),
            &flavor.action("login_reject"),
        ],
    )
    .await;
    match answers.as_deref() {
        Ok([a]) if a["type"] == flavor.action("login_accept") => {
            outcome(&shared.ctx, flavor, id, "login", "model_answer");
            let mut cache = shared.logins.lock().await;
            if cache.len() < 256 {
                cache.insert(key, Instant::now());
            }
            Ok(user.to_owned())
        }
        Ok([a]) if a["type"] == flavor.action("login_reject") => {
            outcome(&shared.ctx, flavor, id, "login", "model_reject");
            Err(unauthorized())
        }
        Ok([]) => {
            outcome(&shared.ctx, flavor, id, "login", "model_silent");
            Err(unauthorized())
        }
        _ => Err(unauthorized()),
    }
}

/// Ask the handler; `Err` (already logged) when it cannot answer.
async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
    allowed: &[&str],
) -> Result<Vec<Value>, ()> {
    let ctx = &shared.ctx;
    let flavor = shared.flavor;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        flavor.protocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, flavor, id, operation, "fail_closed_llm_error");
            return Err(());
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, flavor, id, operation, "fail_closed_invalid_reply");
        return Err(());
    }
    let mut out = Vec::new();
    let mut pending: Vec<ActionResult> = result.protocol_results.into_iter().rev().collect();
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if allowed.contains(&name.as_str()) => {
                out.push(data)
            }
            ActionResult::Multiple(items) => pending.extend(items.into_iter().rev()),
            _ => {}
        }
    }
    Ok(out)
}

fn unavailable() -> Reply {
    plain(
        500,
        crate::utils::wire_failure::WireFailure::Unavailable.text(),
    )
}

/// One request to the handler expecting exactly one answer of the given kinds, or its error.
async fn request(
    shared: &Shared,
    id: ConnectionId,
    data: Value,
    expect: &[&str],
) -> Result<Value, Reply> {
    let flavor = shared.flavor;
    let operation = data["operation"].as_str().unwrap_or("request").to_owned();
    let mut allowed: Vec<String> = expect.iter().map(|e| flavor.action(e)).collect();
    allowed.push(flavor.action("error"));
    let refs: Vec<&str> = allowed.iter().map(String::as_str).collect();
    let answers = ask(
        shared,
        id,
        Event::new(flavor.request_event, data),
        &operation,
        &refs,
    )
    .await
    .map_err(|()| unavailable())?;
    match answers.as_slice() {
        [a] if a["type"] == flavor.action("error") => {
            outcome(&shared.ctx, flavor, id, &operation, "model_reject");
            let status = a["status"].as_u64().unwrap_or(403) as u16;
            Err(match a["precondition"].as_str() {
                Some(c) => precondition(
                    status,
                    if matches!(
                        c,
                        "no-uid-conflict"
                            | "valid-calendar-data"
                            | "supported-calendar-component"
                            | "valid-address-data"
                    ) {
                        flavor.ns()
                    } else {
                        DAV
                    },
                    c,
                ),
                None => plain(status, a["message"].as_str().unwrap_or("refused")),
            })
        }
        [a] => {
            outcome(&shared.ctx, flavor, id, &operation, "model_answer");
            Ok(a.clone())
        }
        [] => {
            outcome(&shared.ctx, flavor, id, &operation, "model_silent");
            Err(unavailable())
        }
        _ => {
            outcome(
                &shared.ctx,
                flavor,
                id,
                &operation,
                "fail_closed_invalid_reply",
            );
            Err(unavailable())
        }
    }
}

#[derive(Clone, Debug)]
struct Collection {
    name: String,
    displayname: String,
    description: String,
    color: Option<String>,
    components: Vec<String>,
}

#[derive(Clone, Debug)]
struct Obj {
    name: String,
    etag: String,
    data: Option<String>,
    component: String,
}

async fn collections(
    shared: &Shared,
    id: ConnectionId,
    user: &str,
) -> Result<Vec<Collection>, Reply> {
    let a = request(
        shared,
        id,
        json!({"operation": "list_collections", "user": user}),
        &["collections"],
    )
    .await?;
    let list = a["collections"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for c in list.iter().take(MAX_COLLECTIONS) {
        let Some(name) = c["name"].as_str().filter(|n| segment_ok(n)) else {
            outcome(
                &shared.ctx,
                shared.flavor,
                id,
                "list_collections",
                "fail_closed_invalid_reply",
            );
            return Err(unavailable());
        };
        let components = c["components"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_ascii_uppercase)
                    .collect()
            })
            .unwrap_or_else(|| vec!["VEVENT".into(), "VTODO".into()]);
        out.push(Collection {
            name: name.to_owned(),
            displayname: c["displayname"].as_str().unwrap_or(name).to_owned(),
            description: c["description"].as_str().unwrap_or("").to_owned(),
            color: c["color"].as_str().map(str::to_owned),
            components,
        });
    }
    Ok(out)
}

async fn objects(
    shared: &Shared,
    id: ConnectionId,
    user: &str,
    coll: &str,
    with_data: bool,
) -> Result<Vec<Obj>, Reply> {
    let a = request(shared, id, json!({"operation": "list_objects", "user": user, "collection": coll, "with_data": with_data}), &["objects"]).await?;
    let mut out = Vec::new();
    for o in a["objects"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .take(MAX_OBJECTS)
    {
        let name = o["name"].as_str().filter(|n| segment_ok(n));
        let data = o["data"].as_str().map(str::to_owned);
        let (Some(name), true) = (name, data.is_some() || !with_data) else {
            outcome(
                &shared.ctx,
                shared.flavor,
                id,
                "list_objects",
                "fail_closed_invalid_reply",
            );
            return Err(unavailable());
        };
        let component = match (&data, shared.flavor.kind) {
            (Some(d), Kind::Calendar) => match object::check_calendar(d) {
                Ok((k, _, _)) => k,
                Err(_) => {
                    outcome(
                        &shared.ctx,
                        shared.flavor,
                        id,
                        "list_objects",
                        "fail_closed_invalid_reply",
                    );
                    return Err(unavailable());
                }
            },
            (Some(d), Kind::AddressBook) => {
                if object::check_vcard(d).is_err() {
                    outcome(
                        &shared.ctx,
                        shared.flavor,
                        id,
                        "list_objects",
                        "fail_closed_invalid_reply",
                    );
                    return Err(unavailable());
                }
                "VCARD".into()
            }
            (None, _) => o["component"].as_str().unwrap_or("VEVENT").to_owned(),
        };
        let etag = o["etag"]
            .as_str()
            .map(quote_etag)
            .or_else(|| data.as_deref().map(etag_of))
            .unwrap_or_else(|| etag_of(name));
        out.push(Obj {
            name: name.to_owned(),
            etag,
            data,
            component,
        });
    }
    Ok(out)
}

fn ctag(objs: &[Obj]) -> String {
    let mut h = std::hash::DefaultHasher::new();
    for o in objs {
        h.write(o.name.as_bytes());
        h.write(o.etag.as_bytes());
    }
    format!("\"c{:016x}\"", h.finish())
}

/// The properties one resource has: (name, value XML). `report_data` adds calendar-data or
/// address-data for REPORT answers.
fn props(
    shared: &Shared,
    t: &Target,
    user: &str,
    coll: Option<&Collection>,
    ctag_value: Option<&str>,
    obj: Option<&Obj>,
    report_data: bool,
) -> Vec<(PropName, String)> {
    let flavor = shared.flavor;
    let p = |ns: &str, n: &str, v: String| ((ns.to_owned(), n.to_owned()), v);
    let href_el = |h: String| format!("<D:href>{}</D:href>", xml::escape(&h));
    let principal = href(flavor, &Target::Principal(user.to_owned()));
    let mut out = vec![p(DAV, "current-user-principal", href_el(principal.clone()))];
    match t {
        Target::Root | Target::Principals => {
            out.push(p(DAV, "resourcetype", "<D:collection/>".into()));
        }
        Target::Principal(u) => {
            out.push(p(
                DAV,
                "resourcetype",
                "<D:principal/><D:collection/>".into(),
            ));
            out.push(p(DAV, "displayname", xml::escape(u)));
            out.push(p(DAV, "principal-URL", href_el(principal.clone())));
            let home = href_el(href(flavor, &Target::Home(u.clone())));
            match flavor.kind {
                Kind::Calendar => {
                    out.push(p(CALDAV, "calendar-home-set", home));
                    out.push(p(
                        CALDAV,
                        "calendar-user-address-set",
                        href_el(format!("mailto:{u}@localhost")),
                    ));
                }
                Kind::AddressBook => out.push(p(CARDDAV, "addressbook-home-set", home)),
            }
        }
        Target::Home(u) => {
            out.push(p(DAV, "resourcetype", "<D:collection/>".into()));
            out.push(p(DAV, "displayname", xml::escape(u)));
            out.push(p(DAV, "owner", href_el(principal.clone())));
        }
        Target::Collection(..) => {
            let c = coll.expect("collection props need the collection");
            let kind = match flavor.kind {
                Kind::Calendar => "<C:calendar/>",
                Kind::AddressBook => "<CR:addressbook/>",
            };
            out.push(p(DAV, "resourcetype", format!("<D:collection/>{kind}")));
            out.push(p(DAV, "displayname", xml::escape(&c.displayname)));
            out.push(p(DAV, "owner", href_el(principal.clone())));
            if let Some(tag) = ctag_value {
                out.push(p(CS, "getctag", xml::escape(tag)));
            }
            match flavor.kind {
                Kind::Calendar => {
                    out.push(p(
                        CALDAV,
                        "calendar-description",
                        xml::escape(&c.description),
                    ));
                    out.push(p(
                        CALDAV,
                        "supported-calendar-component-set",
                        c.components
                            .iter()
                            .map(|k| format!("<C:comp name=\"{}\"/>", xml::escape(k)))
                            .collect(),
                    ));
                    out.push(p(
                        CALDAV,
                        "supported-calendar-data",
                        "<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".into(),
                    ));
                    out.push(p(DAV, "supported-report-set", "<D:supported-report><D:report><C:calendar-query/></D:report></D:supported-report><D:supported-report><D:report><C:calendar-multiget/></D:report></D:supported-report>".into()));
                    if let Some(color) = &c.color {
                        out.push(p(xml::APPLE, "calendar-color", xml::escape(color)));
                    }
                }
                Kind::AddressBook => {
                    out.push(p(
                        CARDDAV,
                        "addressbook-description",
                        xml::escape(&c.description),
                    ));
                    out.push(p(CARDDAV, "supported-address-data", "<CR:address-data-type content-type=\"text/vcard\" version=\"3.0\"/><CR:address-data-type content-type=\"text/vcard\" version=\"4.0\"/>".into()));
                    out.push(p(DAV, "supported-report-set", "<D:supported-report><D:report><CR:addressbook-query/></D:report></D:supported-report><D:supported-report><D:report><CR:addressbook-multiget/></D:report></D:supported-report>".into()));
                }
            }
        }
        Target::Object(..) => {
            let o = obj.expect("object props need the object");
            out.push(p(DAV, "resourcetype", String::new()));
            out.push(p(DAV, "getetag", xml::escape(&o.etag)));
            out.push(p(
                DAV,
                "getcontenttype",
                xml::escape(&flavor.content_type(&o.component)),
            ));
            if let Some(d) = &o.data {
                out.push(p(DAV, "getcontentlength", d.len().to_string()));
                if report_data {
                    let n = match flavor.kind {
                        Kind::Calendar => p(CALDAV, "calendar-data", xml::escape(d)),
                        Kind::AddressBook => p(CARDDAV, "address-data", xml::escape(d)),
                    };
                    out.push(n);
                }
            }
        }
    }
    out
}

fn select(
    available: Vec<(PropName, String)>,
    want: &PropRequest,
) -> (Vec<(PropName, String)>, Vec<PropName>) {
    match want {
        PropRequest::AllProp => (
            available
                .into_iter()
                .filter(|((ns, n), _)| {
                    !(n == "calendar-data" || n == "address-data") || ns.is_empty()
                })
                .collect(),
            vec![],
        ),
        PropRequest::PropName => (
            available
                .into_iter()
                .map(|(k, _)| (k, String::new()))
                .collect(),
            vec![],
        ),
        PropRequest::Props(names) => {
            let mut found = Vec::new();
            let mut missing = Vec::new();
            for n in names {
                match available.iter().find(|(k, _)| k == n) {
                    Some(item) => found.push(item.clone()),
                    None => missing.push(n.clone()),
                }
            }
            (found, missing)
        }
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, Reply> {
    match tokio::time::timeout(
        BODY_TIMEOUT,
        Limited::new(req.into_body(), xml::MAX_BODY).collect(),
    )
    .await
    {
        Ok(Ok(b)) => Ok(b.to_bytes()),
        _ => Err(plain(413, "request body unreadable or over 1 MiB")),
    }
}

fn header_str(req: &Request<Incoming>, name: header::HeaderName) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

async fn route(shared: &Arc<Shared>, id: ConnectionId, req: Request<Incoming>) -> Reply {
    let flavor = shared.flavor;
    let path = req.uri().path().to_owned();
    let method = req.method().clone();
    let allow = "OPTIONS, GET, HEAD, PUT, DELETE, PROPFIND, PROPPATCH, REPORT, MKCOL, MKCALENDAR";
    let dav = match flavor.kind {
        Kind::Calendar => "1, 3, calendar-access",
        Kind::AddressBook => "1, 3, addressbook",
    };
    if path.trim_end_matches('/') == flavor.well_known {
        return reply(301, None, vec![], &[("Location", "/".into())]);
    }
    if method == Method::OPTIONS {
        return reply(
            200,
            None,
            vec![],
            &[("Allow", allow.into()), ("DAV", dav.into())],
        );
    }
    let Some(t) = target(flavor, &path) else {
        return plain(404, "not found");
    };
    let user = match authenticate(shared, id, &req).await {
        Ok(u) => u,
        Err(r) => return r,
    };
    let owner = match &t {
        Target::Principal(u)
        | Target::Home(u)
        | Target::Collection(u, _)
        | Target::Object(u, _, _) => Some(u.clone()),
        _ => None,
    };
    if owner.as_ref().is_some_and(|o| *o != user) {
        return plain(403, "resources of another user");
    }
    let depth = header_str(&req, header::HeaderName::from_static("depth"))
        .unwrap_or_else(|| "infinity".into());
    match method.as_str() {
        "PROPFIND" => {
            let body = match read_body(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let want = match xml::propfind(&body) {
                Ok(w) => w,
                Err(e) => return plain(400, &e.to_string()),
            };
            propfind(shared, id, &t, &user, &want, depth != "0").await
        }
        "REPORT" => {
            let body = match read_body(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let Target::Collection(u, c) = &t else {
                return plain(403, "REPORT is supported on collections");
            };
            match xml::report(&body) {
                Ok(r) => report(shared, id, u, c, r).await,
                Err(e) => precondition(
                    403,
                    DAV,
                    if e.to_string().contains("unsupported REPORT") {
                        "supported-report"
                    } else {
                        "valid-sync-token"
                    },
                ),
            }
        }
        "GET" | "HEAD" => {
            let Target::Object(u, c, o) = &t else {
                return plain(405, "GET is supported on objects");
            };
            match request(
                shared,
                id,
                json!({"operation": "get", "user": u, "collection": c, "name": o}),
                &["object"],
            )
            .await
            {
                Ok(a) => {
                    let Some(data) = a["data"].as_str() else {
                        return unavailable();
                    };
                    let component = match flavor.kind {
                        Kind::Calendar => match object::check_calendar(data) {
                            Ok((k, _, _)) => k,
                            Err(_) => return unavailable(),
                        },
                        Kind::AddressBook => match object::check_vcard(data) {
                            Ok(_) => "VCARD".into(),
                            Err(_) => return unavailable(),
                        },
                    };
                    let etag = a["etag"]
                        .as_str()
                        .map(quote_etag)
                        .unwrap_or_else(|| etag_of(data));
                    let body = if method == Method::HEAD {
                        vec![]
                    } else {
                        data.as_bytes().to_vec()
                    };
                    reply(
                        200,
                        Some(&flavor.content_type(&component)),
                        body,
                        &[("ETag", etag)],
                    )
                }
                Err(r) => r,
            }
        }
        "PUT" => {
            let Target::Object(u, c, o) = &t else {
                return plain(405, "PUT is supported on objects");
            };
            let (if_match, if_none_match) = (
                header_str(&req, header::IF_MATCH),
                header_str(&req, header::IF_NONE_MATCH),
            );
            let body = match read_body(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let Ok(text) = String::from_utf8(body.to_vec()) else {
                return precondition(
                    403,
                    flavor.ns(),
                    if flavor.kind == Kind::Calendar {
                        "valid-calendar-data"
                    } else {
                        "valid-address-data"
                    },
                );
            };
            let checked = match flavor.kind {
                Kind::Calendar => object::check_calendar(&text).map(|(k, uid, _)| (k, uid)),
                Kind::AddressBook => {
                    object::check_vcard(&text).map(|(uid, _)| ("VCARD".to_owned(), uid))
                }
            };
            let (component, uid) = match checked {
                Ok(v) => v,
                Err(e) => {
                    Log::new(Some(&shared.ctx.status_tx))
                        .warn(format!("{} PUT refused: {e}", flavor.name));
                    return precondition(
                        403,
                        flavor.ns(),
                        if flavor.kind == Kind::Calendar {
                            "valid-calendar-data"
                        } else {
                            "valid-address-data"
                        },
                    );
                }
            };
            match request(shared, id, json!({"operation": "put", "user": u, "collection": c, "name": o, "data": text, "uid": uid, "component": component, "if_match": if_match, "if_none_match": if_none_match}), &["stored"]).await {
                Ok(a) => {
                    let etag = a["etag"].as_str().map(quote_etag).unwrap_or_else(|| etag_of(&text));
                    let created = a["created"].as_bool().unwrap_or(if_none_match.as_deref() == Some("*"));
                    reply(if created { 201 } else { 204 }, None, vec![], &[("ETag", etag)])
                }
                Err(r) => r,
            }
        }
        "DELETE" => {
            let (coll, name) = match &t {
                Target::Object(_, c, o) => (c.clone(), Some(o.clone())),
                Target::Collection(_, c) => (c.clone(), None),
                _ => return plain(403, "DELETE is supported on collections and objects"),
            };
            match request(shared, id, json!({"operation": "delete", "user": user, "collection": coll, "name": name, "if_match": header_str(&req, header::IF_MATCH)}), &["done"]).await {
                Ok(_) => reply(204, None, vec![], &[]),
                Err(r) => r,
            }
        }
        "MKCALENDAR" | "MKCOL" => {
            let Target::Collection(u, c) = &t else {
                return plain(403, "collections are created under the home");
            };
            let body = match read_body(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let mut displayname = None;
            let mut description = None;
            let mut components = Vec::new();
            if !body.iter().all(u8::is_ascii_whitespace) {
                let Ok(root) = xml::parse(&body) else {
                    return plain(400, "malformed body");
                };
                if let Some(prop) = root.child(DAV, "set").and_then(|s| s.child(DAV, "prop")) {
                    displayname = prop.child(DAV, "displayname").map(|d| d.text.clone());
                    description = prop
                        .child(CALDAV, "calendar-description")
                        .or(prop.child(CARDDAV, "addressbook-description"))
                        .map(|d| d.text.clone());
                    if let Some(set) = prop.child(CALDAV, "supported-calendar-component-set") {
                        components = set
                            .children_named(CALDAV, "comp")
                            .filter_map(|c| c.attr("name").map(str::to_owned))
                            .collect();
                    }
                    if method.as_str() == "MKCOL" && flavor.kind == Kind::AddressBook {
                        let is_book = prop
                            .child(DAV, "resourcetype")
                            .is_some_and(|r| r.child(CARDDAV, "addressbook").is_some());
                        if !is_book {
                            return precondition(403, DAV, "valid-resourcetype");
                        }
                    }
                }
            }
            if (method.as_str() == "MKCALENDAR") != (flavor.kind == Kind::Calendar) {
                return plain(
                    405,
                    "use MKCALENDAR for calendars and extended MKCOL for address books",
                );
            }
            match request(shared, id, json!({"operation": "make_collection", "user": u, "collection": c, "displayname": displayname, "description": description, "components": components}), &["done"]).await {
                Ok(_) => reply(201, None, vec![], &[]),
                Err(r) => r,
            }
        }
        "PROPPATCH" => {
            let body = match read_body(req).await {
                Ok(b) => b,
                Err(r) => return r,
            };
            let Ok(root) = xml::parse(&body) else {
                return plain(400, "malformed body");
            };
            let mut set = serde_json::Map::new();
            let mut names = Vec::new();
            for s in root.children_named(DAV, "set") {
                for p in s
                    .child(DAV, "prop")
                    .map(|p| p.children.clone())
                    .unwrap_or_default()
                {
                    names.push((p.ns.clone(), p.name.clone()));
                    set.insert(format!("{{{}}}{}", p.ns, p.name), json!(p.text));
                }
            }
            let mut removed = Vec::new();
            for s in root.children_named(DAV, "remove") {
                for p in s
                    .child(DAV, "prop")
                    .map(|p| p.children.clone())
                    .unwrap_or_default()
                {
                    names.push((p.ns.clone(), p.name.clone()));
                    removed.push(json!(format!("{{{}}}{}", p.ns, p.name)));
                }
            }
            match request(shared, id, json!({"operation": "proppatch", "path": href(flavor, &t), "set": set, "remove": removed}), &["done"]).await {
                Ok(_) => multistatus(&[xml::response(&href(flavor, &t), &names.into_iter().map(|n| (n, String::new())).collect::<Vec<_>>(), &[])]),
                Err(r) => r,
            }
        }
        _ => reply(405, None, vec![], &[("Allow", allow.into())]),
    }
}

async fn propfind(
    shared: &Arc<Shared>,
    id: ConnectionId,
    t: &Target,
    user: &str,
    want: &PropRequest,
    depth1: bool,
) -> Reply {
    let flavor = shared.flavor;
    let mut responses = Vec::new();
    let own = |t: &Target, coll: Option<&Collection>, tag: Option<&str>, obj: Option<&Obj>| {
        let (found, missing) = select(props(shared, t, user, coll, tag, obj, false), want);
        xml::response(&href(flavor, t), &found, &missing)
    };
    match t {
        Target::Root => responses.push(own(t, None, None, None)),
        Target::Principals => {
            responses.push(own(t, None, None, None));
            if depth1 {
                responses.push(own(&Target::Principal(user.to_owned()), None, None, None));
            }
        }
        Target::Principal(_) => responses.push(own(t, None, None, None)),
        Target::Home(u) => {
            responses.push(own(t, None, None, None));
            if depth1 {
                let colls = match collections(shared, id, u).await {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                for c in &colls {
                    responses.push(own(
                        &Target::Collection(u.clone(), c.name.clone()),
                        Some(c),
                        None,
                        None,
                    ));
                }
            }
        }
        Target::Collection(u, c) => {
            let colls = match collections(shared, id, u).await {
                Ok(c) => c,
                Err(r) => return r,
            };
            let Some(coll) = colls.iter().find(|x| x.name == *c) else {
                return plain(404, "no such collection");
            };
            let objs = match objects(shared, id, u, c, false).await {
                Ok(o) => o,
                Err(r) => return r,
            };
            let tag = ctag(&objs);
            responses.push(own(t, Some(coll), Some(&tag), None));
            if depth1 {
                for o in &objs {
                    responses.push(own(
                        &Target::Object(u.clone(), c.clone(), o.name.clone()),
                        None,
                        None,
                        Some(o),
                    ));
                }
            }
        }
        Target::Object(u, c, name) => {
            let objs = match objects(shared, id, u, c, false).await {
                Ok(o) => o,
                Err(r) => return r,
            };
            let Some(o) = objs.iter().find(|o| o.name == *name) else {
                return plain(404, "no such object");
            };
            responses.push(own(t, None, None, Some(o)));
        }
    }
    multistatus(&responses)
}

async fn report(
    shared: &Arc<Shared>,
    id: ConnectionId,
    user: &str,
    coll: &str,
    r: Report,
) -> Reply {
    let flavor = shared.flavor;
    let objs = match objects(shared, id, user, coll, true).await {
        Ok(o) => o,
        Err(r) => return r,
    };
    let respond = |o: &Obj, want: &PropRequest| {
        let t = Target::Object(user.to_owned(), coll.to_owned(), o.name.clone());
        let (found, missing) = select(props(shared, &t, user, None, None, Some(o), true), want);
        xml::response(&href(flavor, &t), &found, &missing)
    };
    let mut responses = Vec::new();
    match r {
        Report::CalendarQuery {
            props: want,
            filter,
        } if flavor.kind == Kind::Calendar => {
            for o in &objs {
                let Some(Ok((_, _, cal))) = o.data.as_deref().map(object::check_calendar) else {
                    continue;
                };
                if filter
                    .child(CALDAV, "comp-filter")
                    .is_none_or(|f| comp_filter(&cal, f))
                {
                    responses.push(respond(o, &want));
                }
            }
        }
        Report::AddressbookQuery {
            props: want,
            filter,
            limit,
        } if flavor.kind == Kind::AddressBook => {
            for o in &objs {
                let Some(Ok((_, card))) = o.data.as_deref().map(object::check_vcard) else {
                    continue;
                };
                if card_filter(&card, &filter) {
                    responses.push(respond(o, &want));
                    if limit.is_some_and(|l| responses.len() >= l) {
                        break;
                    }
                }
            }
        }
        Report::CalendarMultiget { props: want, hrefs }
        | Report::AddressbookMultiget { props: want, hrefs } => {
            let base = href(
                flavor,
                &Target::Collection(user.to_owned(), coll.to_owned()),
            );
            for h in hrefs.iter().take(MAX_OBJECTS) {
                let path = h
                    .split("://")
                    .nth(1)
                    .and_then(|r| r.find('/').map(|i| r[i..].to_owned()))
                    .unwrap_or_else(|| h.clone());
                let path = urlencoding::decode(&path)
                    .map(|p| p.into_owned())
                    .unwrap_or(path);
                match path
                    .strip_prefix(&base)
                    .and_then(|name| objs.iter().find(|o| o.name == name))
                {
                    Some(o) => responses.push(respond(o, &want)),
                    None => responses.push(xml::status_response(h, "404 Not Found")),
                }
            }
        }
        _ => return precondition(403, DAV, "supported-report"),
    }
    multistatus(&responses)
}

fn time_range(el: &xml::El) -> (Option<i64>, Option<i64>) {
    let t = |k: &str| el.attr(k).and_then(|v| object::timestamp(v).ok());
    (t("start"), t("end"))
}

fn text_matches(value: &str, tm: &xml::El) -> bool {
    object::text_match(
        value,
        tm.text.trim(),
        tm.attr("match-type").unwrap_or("contains"),
        tm.attr("negate-condition") == Some("yes"),
    )
}

/// RFC 4791 §9.7.1: a comp-filter matches a component with that name whose time-range,
/// prop-filters and nested comp-filters all hold.
fn comp_filter(c: &Component, f: &xml::El) -> bool {
    if !f
        .attr("name")
        .is_some_and(|n| n.eq_ignore_ascii_case(&c.name))
    {
        return false;
    }
    if let Some(tr) = f.child(CALDAV, "time-range") {
        let (s, e) = time_range(tr);
        if !object::overlaps(c, s, e) {
            return false;
        }
    }
    for pf in f.children_named(CALDAV, "prop-filter") {
        let name = pf.attr("name").unwrap_or("");
        let mut values = c.props_named(name).peekable();
        if pf.child(CALDAV, "is-not-defined").is_some() {
            if values.peek().is_some() {
                return false;
            }
            continue;
        }
        if values.peek().is_none() {
            return false;
        }
        if let Some(tm) = pf.child(CALDAV, "text-match") {
            if !values.any(|p| text_matches(&p.value, tm)) {
                return false;
            }
        }
    }
    for cf in f.children_named(CALDAV, "comp-filter") {
        let wanted = cf.attr("name").unwrap_or("");
        let mut matching = c
            .children
            .iter()
            .filter(|ch| ch.name.eq_ignore_ascii_case(wanted));
        if cf.child(CALDAV, "is-not-defined").is_some() {
            if matching.next().is_some() {
                return false;
            }
        } else if !matching.any(|ch| comp_filter(ch, cf)) {
            return false;
        }
    }
    true
}

/// RFC 6352 §10.5: prop-filters combined by the filter's test (anyof, the default, or allof).
fn card_filter(card: &Component, filter: &xml::El) -> bool {
    let pfs: Vec<&xml::El> = filter.children_named(CARDDAV, "prop-filter").collect();
    if pfs.is_empty() {
        return true;
    }
    let one = |pf: &xml::El| -> bool {
        let name = pf.attr("name").unwrap_or("");
        let values: Vec<&object::Property> = card.props_named(name).collect();
        if pf.child(CARDDAV, "is-not-defined").is_some() {
            return values.is_empty();
        }
        let tms: Vec<&xml::El> = pf.children_named(CARDDAV, "text-match").collect();
        if tms.is_empty() {
            return !values.is_empty();
        }
        let test = |tm: &&xml::El| values.iter().any(|p| text_matches(&p.value, tm));
        if pf.attr("test") == Some("allof") {
            tms.iter().all(test)
        } else {
            tms.iter().any(test)
        }
    };
    if filter.attr("test") == Some("allof") {
        pfs.into_iter().all(one)
    } else {
        pfs.into_iter().any(one)
    }
}
