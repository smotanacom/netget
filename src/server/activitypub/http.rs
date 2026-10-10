//! The HTTP surface an ActivityPub instance serves, for either role: WebFinger, NodeInfo,
//! actor documents, followers/following/outbox collections, objects, and the inboxes —
//! where every POST is verified (HTTP Signature, Digest, Date, actor = signer) before it
//! is queued for whoever answers it.
use super::instance::{Inbound, Instance, MAX_DOCUMENT};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{body::Incoming, header::CONTENT_TYPE, Method, Request, Response, StatusCode};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub type Reply = Response<Full<Bytes>>;
pub const ACTIVITY_JSON: &str = "application/activity+json";

fn reply(status: u16, content_type: &str, body: Vec<u8>) -> Reply {
    let mut r = Response::new(Full::new(Bytes::from(body)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if let Ok(v) = hyper::header::HeaderValue::from_str(content_type) {
        r.headers_mut().insert(CONTENT_TYPE, v);
    }
    r
}

fn json_reply(status: u16, content_type: &str, v: &Value) -> Reply {
    reply(
        status,
        content_type,
        serde_json::to_vec(v).unwrap_or_default(),
    )
}

fn error(status: u16, message: &str) -> Reply {
    json_reply(status, "application/json", &json!({"error": message}))
}

/// What happened to a request, for the caller's log.
pub enum Outcome {
    Served,
    Queued,
    Refused(String),
}

pub async fn handle(
    instance: &Arc<Instance>,
    request: Request<Incoming>,
    inbound: &mpsc::Sender<Inbound>,
) -> (Reply, Outcome) {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().unwrap_or_default().to_string();
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let served = |r: Reply| (r, Outcome::Served);
    match (&parts.method, segs.as_slice()) {
        (&Method::GET, [".well-known", "webfinger"]) => {
            let resource = query
                .split('&')
                .find_map(|p| p.strip_prefix("resource="))
                .map(|r| {
                    urlencoding::decode(r)
                        .map(|c| c.into_owned())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            served(match instance.webfinger(&resource) {
                Some(j) => json_reply(200, "application/jrd+json", &j),
                None => error(404, "no such account"),
            })
        }
        (&Method::GET, [".well-known", "nodeinfo"]) => served(json_reply(
            200,
            "application/json",
            &json!({"links": [{"rel": "http://nodeinfo.diaspora.software/ns/schema/2.1",
                              "href": format!("{}/nodeinfo/2.1", instance.base)}]}),
        )),
        (&Method::GET, ["nodeinfo", "2.1"]) => {
            let users = instance
                .actors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            served(json_reply(
                200,
                "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.1#\"",
                &json!({"version": "2.1",
                        "software": {"name": "netget", "version": env!("CARGO_PKG_VERSION")},
                        "protocols": ["activitypub"], "services": {"inbound": [], "outbound": []},
                        "openRegistrations": false,
                        "usage": {"users": {"total": users}}, "metadata": {}}),
            ))
        }
        (&Method::GET, ["users", name]) => served(match instance.actor_document(name) {
            Some(d) => json_reply(200, ACTIVITY_JSON, &d),
            None => error(404, "no such actor"),
        }),
        (&Method::GET, ["users", name, which @ ("followers" | "following" | "outbox")]) => {
            served(match instance.collection(name, which) {
                Some(c) => json_reply(200, ACTIVITY_JSON, &c),
                None => error(404, "no such actor"),
            })
        }
        (&Method::GET, ["notes", _]) => {
            let id = format!("{}{}", instance.base, path);
            served(match instance.object(&id) {
                Some(o) => json_reply(200, ACTIVITY_JSON, &o),
                None => error(404, "no such object"),
            })
        }
        (&Method::POST, ["users", name, "inbox"]) | (&Method::POST, [name @ "inbox"]) => {
            let to = (*name != "inbox").then(|| name.to_string());
            if let Some(n) = &to {
                if !instance.has_actor(n) {
                    return served(error(404, "no such actor"));
                }
            }
            let bytes = match Limited::new(body, MAX_DOCUMENT).collect().await {
                Ok(b) => b.to_bytes(),
                Err(_) => {
                    return (
                        error(413, "activity too large"),
                        Outcome::Refused("too large".into()),
                    )
                }
            };
            let headers: HashMap<String, String> = parts
                .headers
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|v| (k.as_str().to_string(), v.to_string()))
                })
                .collect();
            match instance.verify_inbound(to, &path, &headers, &bytes).await {
                Err(e) => {
                    let why = format!("{e:#}");
                    (error(401, &why), Outcome::Refused(why))
                }
                Ok(inb) => match inbound.try_send(inb) {
                    Ok(()) => (reply(202, "text/plain", Vec::new()), Outcome::Queued),
                    Err(_) => (
                        error(503, "inbox busy"),
                        Outcome::Refused("queue full".into()),
                    ),
                },
            }
        }
        _ => served(error(404, "not found")),
    }
}
