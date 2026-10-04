//! The DAV client engine CalDAV and CardDAV share: RFC 6764 discovery, PROPFIND, REPORT
//! queries and multistatus parsing, object GET/PUT/DELETE with preconditions, and collection
//! creation. Outgoing objects are validated with the server's own parser.
use super::object;
use super::server::Kind;
use super::xml::{self, CALDAV, CARDDAV, CS, DAV};
use crate::client::http_fetch::FetchClient;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event, EventType};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
use anyhow::{bail, ensure, Context, Result};
use base64::Engine;
use hyper::Method;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::mpsc;

pub const DEFAULT_SCHEME: &str = "https";
const TIMEOUT: Duration = Duration::from_secs(30);

pub struct ClientFlavor {
    pub kind: Kind,
    pub name: &'static str,
    pub prefix: &'static str,
    pub connected_event: &'static LazyLock<EventType>,
    pub response_event: &'static LazyLock<EventType>,
    pub protocol: &'static (dyn Client + Sync),
}

impl ClientFlavor {
    fn home_prop(&self) -> (&'static str, &'static str) {
        match self.kind {
            Kind::Calendar => (CALDAV, "calendar-home-set"),
            Kind::AddressBook => (CARDDAV, "addressbook-home-set"),
        }
    }
    fn collection_marker(&self) -> (&'static str, &'static str) {
        match self.kind {
            Kind::Calendar => (CALDAV, "calendar"),
            Kind::AddressBook => (CARDDAV, "addressbook"),
        }
    }
    fn content_type(&self) -> &'static str {
        match self.kind {
            Kind::Calendar => "text/calendar; charset=utf-8",
            Kind::AddressBook => "text/vcard; charset=utf-8",
        }
    }
}

struct Conn {
    fetch: FetchClient,
    base: String,
    auth: Option<String>,
}

struct Answer {
    status: u16,
    etag: Option<String>,
    location: Option<String>,
    body: String,
}

impl Conn {
    async fn send(
        &self,
        method: &str,
        path: &str,
        depth: Option<&str>,
        body: Option<(&str, String)>,
        headers: &[(&str, String)],
    ) -> Result<Answer> {
        let m = Method::from_bytes(method.as_bytes())?;
        let url = if path.starts_with("http") {
            path.to_owned()
        } else {
            format!("{}{path}", self.base)
        };
        let mut r = self.fetch.request(m, &url);
        if let Some(a) = &self.auth {
            r = r.header("Authorization", format!("Basic {a}"));
        }
        if let Some(d) = depth {
            r = r.header("Depth", d);
        }
        for (k, v) in headers {
            r = r.header(*k, v);
        }
        if let Some((ct, b)) = body {
            r = r.header("Content-Type", ct).body(b);
        }
        let resp = r.send().await?;
        let status = resp.status().as_u16();
        let h = |n: &str| {
            resp.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (etag, location) = (h("etag"), h("location"));
        let body = String::from_utf8_lossy(&resp.bytes().await?).into_owned();
        Ok(Answer {
            status,
            etag,
            location,
            body,
        })
    }

    async fn propfind(
        &self,
        path: &str,
        depth: &str,
        props: &[(&str, &str)],
    ) -> Result<Vec<(String, xml::El)>> {
        let inner: String = props
            .iter()
            .map(|(ns, n)| format!("<{} xmlns=\"{ns}\"/>", n))
            .collect();
        let body = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><prop>{inner}</prop></propfind>");
        let a = self
            .send(
                "PROPFIND",
                path,
                Some(depth),
                Some(("application/xml; charset=utf-8", body)),
                &[],
            )
            .await?;
        ensure!(
            a.status == 207,
            "PROPFIND {path} answered HTTP {}",
            a.status
        );
        responses(&a.body)
    }
}

/// `(href, merged 200 props)` for each `<D:response>` of a multistatus body.
fn responses(body: &str) -> Result<Vec<(String, xml::El)>> {
    let root = xml::parse(body.as_bytes())?;
    ensure!(root.is(DAV, "multistatus"), "expected a multistatus body");
    let mut out = Vec::new();
    for r in root.children_named(DAV, "response") {
        let href = r
            .child(DAV, "href")
            .map(|h| h.text.trim().to_owned())
            .unwrap_or_default();
        let mut merged = xml::El {
            ns: DAV.into(),
            name: "prop".into(),
            ..Default::default()
        };
        for ps in r.children_named(DAV, "propstat") {
            let ok = ps
                .child(DAV, "status")
                .is_some_and(|s| s.text.contains(" 200"));
            if let (true, Some(p)) = (ok, ps.child(DAV, "prop")) {
                merged.children.extend(p.children.iter().cloned());
            }
        }
        out.push((href, merged));
    }
    Ok(out)
}

fn path_of(href: &str) -> String {
    match href.split_once("://") {
        Some((_, rest)) => rest
            .find('/')
            .map(|i| rest[i..].to_owned())
            .unwrap_or_else(|| "/".into()),
        None => href.to_owned(),
    }
}

fn href_in(p: &xml::El, ns: &str, name: &str) -> Option<String> {
    p.child(ns, name)
        .and_then(|e| e.child(DAV, "href"))
        .map(|h| path_of(h.text.trim()))
}

async fn discover(conn: &Conn, flavor: &ClientFlavor) -> Result<(String, String)> {
    // RFC 6764 §5: the well-known URL redirects to the context path; follow up to three hops.
    let mut path = format!("/.well-known/{}", flavor.prefix);
    let mut principal = None;
    for _ in 0..3 {
        let a = conn.send("PROPFIND", &path, Some("0"), Some(("application/xml; charset=utf-8", "<?xml version=\"1.0\"?><propfind xmlns=\"DAV:\"><prop><current-user-principal/></prop></propfind>".into())), &[]).await?;
        match a.status {
            301 | 302 | 307 | 308 => {
                path = path_of(&a.location.context("redirect without Location")?)
            }
            207 => {
                principal = responses(&a.body)?
                    .first()
                    .and_then(|(_, p)| href_in(p, DAV, "current-user-principal"));
                break;
            }
            _ => break,
        }
    }
    if principal.is_none() {
        principal = conn
            .propfind("/", "0", &[(DAV, "current-user-principal")])
            .await
            .ok()
            .and_then(|r| {
                r.first()
                    .and_then(|(_, p)| href_in(p, DAV, "current-user-principal"))
            });
    }
    let principal = principal.context("the server names no current-user-principal")?;
    let (ns, n) = flavor.home_prop();
    let home = conn
        .propfind(&principal, "0", &[(ns, n)])
        .await?
        .first()
        .and_then(|(_, p)| href_in(p, ns, n))
        .with_context(|| format!("the principal has no {n}"))?;
    Ok((principal, home))
}

async fn collections(conn: &Conn, flavor: &ClientFlavor, home: &str) -> Result<Vec<Value>> {
    let (mns, mname) = flavor.collection_marker();
    let found = conn
        .propfind(
            home,
            "1",
            &[(DAV, "resourcetype"), (DAV, "displayname"), (CS, "getctag")],
        )
        .await?;
    Ok(found
        .into_iter()
        .filter(|(_, p)| p.child(DAV, "resourcetype").is_some_and(|r| r.child(mns, mname).is_some()))
        .map(|(h, p)| {
            let path = path_of(&h);
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_owned();
            json!({"href": path, "name": name, "displayname": p.child(DAV, "displayname").map(|d| d.text.clone()), "ctag": p.child(CS, "getctag").map(|d| d.text.clone())})
        })
        .collect())
}

pub async fn connect(ctx: ConnectContext, flavor: &'static ClientFlavor) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let opt = |k: &str| -> Result<Option<String>> {
        Ok(p.map(|p| p.get_optional_string(k)).transpose()?.flatten())
    };
    let scheme = opt("scheme")?.unwrap_or_else(|| DEFAULT_SCHEME.to_owned());
    ensure!(
        scheme == "http" || scheme == "https",
        "scheme must be http or https"
    );
    let base = format!("{scheme}://{}", ctx.remote_addr);
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
    let auth = match opt("username")? {
        Some(u) => Some(
            base64::engine::general_purpose::STANDARD
                .encode(format!("{u}:{}", opt("password")?.unwrap_or_default())),
        ),
        None => None,
    };
    let conn = Conn {
        fetch: fetch
            .with_max_body(xml::MAX_BODY * 4)
            .with_user_agent("netget-dav"),
        base,
        auth,
    };
    let (principal, home) = discover(&conn, flavor)
        .await
        .context("discovering the principal and home")?;
    let colls = collections(&conn, flavor, &home).await?;
    let local: SocketAddr = "0.0.0.0:0".parse()?;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        flavor.connected_event,
        json!({"principal": principal, "home": home, "collections": colls}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
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
                flavor.protocol,
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
                    .warn(format!("{} client handler: {e}", flavor.name)),
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
            flavor,
            &conn,
            &home,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("{} client ended: {e}", flavor.name));
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
    flavor: &ClientFlavor,
    conn: &Conn,
    home: &str,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        match flavor.protocol.execute_action(action.clone()) {
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
        let outcome = perform(flavor, conn, home, &action).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    flavor.name,
                    None,
                    "injected_action",
                    json!({"type": action["type"], "collection": action["collection"]}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                reply(command, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                events
                    .send(Event::new(flavor.response_event, data))
                    .await
                    .context("DAV event consumer stopped")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("{} request failed: {e}", flavor.name));
                reply(command, Err(e));
            }
        }
    }
}

fn collection_path(home: &str, coll: &str) -> String {
    if coll.starts_with('/') {
        format!("{}/", coll.trim_end_matches('/'))
    } else {
        format!("{}/{}/", home.trim_end_matches('/'), coll)
    }
}

fn objects_from(
    found: Vec<(String, xml::El)>,
    coll: &str,
    data: bool,
    flavor: &ClientFlavor,
) -> Vec<Value> {
    let (dns, dname) = match flavor.kind {
        Kind::Calendar => (CALDAV, "calendar-data"),
        Kind::AddressBook => (CARDDAV, "address-data"),
    };
    found
        .into_iter()
        .map(|(h, p)| (path_of(&h), p))
        .filter(|(h, _)| h.trim_end_matches('/') != coll.trim_end_matches('/'))
        .map(|(h, p)| {
            let name = h.rsplit('/').next().unwrap_or("").to_owned();
            let mut o = json!({"href": h, "name": name, "etag": p.child(DAV, "getetag").map(|e| e.text.clone())});
            if data {
                o["data"] = json!(p.child(dns, dname).map(|d| d.text.clone()));
            }
            o
        })
        .collect()
}

async fn perform(flavor: &ClientFlavor, conn: &Conn, home: &str, a: &Value) -> Result<Value> {
    let op = a["type"]
        .as_str()
        .unwrap_or_default()
        .strip_prefix(&format!("{}_", flavor.prefix))
        .unwrap_or_default()
        .to_owned();
    let coll = collection_path(home, a["collection"].as_str().unwrap_or_default());
    let item = format!("{coll}{}", a["name"].as_str().unwrap_or_default());
    let mut out = json!({"operation": op});
    let status = match op.as_str() {
        "list" => {
            let found = conn
                .propfind(&coll, "1", &[(DAV, "getetag"), (DAV, "getcontenttype")])
                .await?;
            out["objects"] = json!(objects_from(found, &coll, false, flavor));
            207
        }
        "get" => {
            let r = conn.send("GET", &item, None, None, &[]).await?;
            if r.status == 200 {
                out["data"] = json!(r.body);
                out["etag"] = json!(r.etag);
            }
            r.status
        }
        "put" => {
            let data = a["data"].as_str().unwrap_or_default().to_owned();
            let mut headers = Vec::new();
            if let Some(e) = a["if_match"].as_str() {
                headers.push(("If-Match", e.to_owned()));
            }
            if a["create_only"].as_bool().unwrap_or(false) {
                headers.push(("If-None-Match", "*".to_owned()));
            }
            let r = conn
                .send(
                    "PUT",
                    &item,
                    None,
                    Some((flavor.content_type(), data)),
                    &headers,
                )
                .await?;
            out["etag"] = json!(r.etag);
            if r.status >= 400 {
                out["error"] = json!(r.body.chars().take(512).collect::<String>());
            }
            r.status
        }
        "delete" => {
            let headers: Vec<(&str, String)> = a["if_match"]
                .as_str()
                .map(|e| vec![("If-Match", e.to_owned())])
                .unwrap_or_default();
            let r = conn
                .send(
                    "DELETE",
                    if a["name"].is_string() { &item } else { &coll },
                    None,
                    None,
                    &headers,
                )
                .await?;
            r.status
        }
        "query" => {
            let body = match flavor.kind {
                Kind::Calendar => {
                    let comp = a["component"].as_str().unwrap_or("VEVENT");
                    let range = match (a["start"].as_str(), a["end"].as_str()) {
                        (None, None) => String::new(),
                        (s, e) => format!(
                            "<C:time-range{}{}/>",
                            s.map(|s| format!(" start=\"{}\"", xml::escape(s)))
                                .unwrap_or_default(),
                            e.map(|e| format!(" end=\"{}\"", xml::escape(e)))
                                .unwrap_or_default()
                        ),
                    };
                    format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><C:calendar-query xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\"><D:prop><D:getetag/><C:calendar-data/></D:prop><C:filter><C:comp-filter name=\"VCALENDAR\"><C:comp-filter name=\"{}\">{range}</C:comp-filter></C:comp-filter></C:filter></C:calendar-query>", xml::escape(comp))
                }
                Kind::AddressBook => {
                    let prop = a["property"].as_str().unwrap_or("FN");
                    let mt = a["match_type"].as_str().unwrap_or("contains");
                    format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><CR:addressbook-query xmlns:D=\"DAV:\" xmlns:CR=\"{CARDDAV}\"><D:prop><D:getetag/><CR:address-data/></D:prop><CR:filter><CR:prop-filter name=\"{}\"><CR:text-match collation=\"i;unicode-casemap\" match-type=\"{}\">{}</CR:text-match></CR:prop-filter></CR:filter></CR:addressbook-query>", xml::escape(prop), xml::escape(mt), xml::escape(a["text"].as_str().unwrap_or_default()))
                }
            };
            let r = conn
                .send(
                    "REPORT",
                    &coll,
                    Some("1"),
                    Some(("application/xml; charset=utf-8", body)),
                    &[],
                )
                .await?;
            if r.status == 207 {
                out["objects"] = json!(objects_from(responses(&r.body)?, &coll, true, flavor));
            }
            r.status
        }
        "make_collection" => {
            let name = a["displayname"]
                .as_str()
                .map(|d| format!("<D:displayname>{}</D:displayname>", xml::escape(d)))
                .unwrap_or_default();
            let (method, body) = match flavor.kind {
                Kind::Calendar => ("MKCALENDAR", format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><C:mkcalendar xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\"><D:set><D:prop>{name}</D:prop></D:set></C:mkcalendar>")),
                Kind::AddressBook => ("MKCOL", format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:mkcol xmlns:D=\"DAV:\" xmlns:CR=\"{CARDDAV}\"><D:set><D:prop><D:resourcetype><D:collection/><CR:addressbook/></D:resourcetype>{name}</D:prop></D:set></D:mkcol>")),
            };
            conn.send(
                method,
                &coll,
                None,
                Some(("application/xml; charset=utf-8", body)),
                &[],
            )
            .await?
            .status
        }
        other => bail!("unsupported operation {other}"),
    };
    out["status"] = json!(status);
    Ok(out)
}

/// Shape checks shared by both clients' `execute_action`.
pub fn check_action(flavor_prefix: &str, kind: Kind, v: &Value) -> Result<ClientActionResult> {
    let ty = v["type"].as_str().unwrap_or_default();
    if ty == "disconnect" {
        return Ok(ClientActionResult::Disconnect);
    }
    let op = ty
        .strip_prefix(&format!("{flavor_prefix}_"))
        .context("unknown DAV client action")?;
    ensure!(
        matches!(
            op,
            "list" | "get" | "put" | "delete" | "query" | "make_collection"
        ),
        "unknown DAV client action {ty}"
    );
    let coll = v["collection"].as_str().context("collection is required")?;
    ensure!(
        !coll.is_empty()
            && coll.len() <= 512
            && !coll.contains("..")
            && !coll.chars().any(char::is_control),
        "collection is a name or an absolute path"
    );
    if matches!(op, "get" | "put") {
        ensure!(
            v["name"].as_str().is_some_and(super::server::segment_ok),
            "name must be a simple file name"
        );
    }
    if op == "put" {
        let data = v["data"].as_str().context("data is required")?;
        match kind {
            Kind::Calendar => {
                object::check_calendar(data)?;
            }
            Kind::AddressBook => {
                object::check_vcard(data)?;
            }
        }
    }
    if op == "query" && kind == Kind::Calendar {
        for k in ["start", "end"] {
            if let Some(t) = v[k].as_str() {
                ensure!(
                    t.ends_with('Z') && object::timestamp(t).is_ok(),
                    "{k} is a UTC date-time like 20261005T000000Z"
                );
            }
        }
    }
    Ok(ClientActionResult::Custom {
        name: ty.into(),
        data: v.clone(),
    })
}
