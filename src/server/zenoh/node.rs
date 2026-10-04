//! What the Zenoh server and client share: session configuration, sample and payload JSON,
//! and carrying out the handler's put, delete, get and reply actions on a session.
use ::zenoh::bytes::{Encoding, ZBytes};
use ::zenoh::query::{Query, QueryTarget};
use ::zenoh::sample::{Sample, SampleKind};
use ::zenoh::Session;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value as Json};
use std::time::Duration;

/// Bytes of payload one action may carry.
pub const MAX_PAYLOAD: usize = 1024 * 1024;
/// Replies collected for one get; key expressions per startup list.
pub const MAX_REPLIES: usize = 256;
pub const MAX_KEYS: usize = 64;
pub const GET_TIMEOUT: Duration = Duration::from_secs(10);

/// A session configuration with multicast scouting off: only the given endpoints.
pub fn config(mode: &str, listen: Option<&str>, connect: Option<&str>) -> Result<::zenoh::Config> {
    ensure!(
        matches!(mode, "router" | "peer" | "client"),
        "mode is router, peer or client"
    );
    let mut c = ::zenoh::Config::default();
    let set = |c: &mut ::zenoh::Config, k: &str, v: String| {
        c.insert_json5(k, &v)
            .map_err(|e| anyhow::anyhow!("zenoh config {k}: {e}"))
    };
    set(&mut c, "mode", format!("\"{mode}\""))?;
    set(&mut c, "scouting/multicast/enabled", "false".into())?;
    set(
        &mut c,
        "listen/endpoints",
        match listen {
            Some(l) => format!("[\"{l}\"]"),
            None => "[]".into(),
        },
    )?;
    set(
        &mut c,
        "connect/endpoints",
        match connect {
            Some(e) => format!("[\"{e}\"]"),
            None => "[]".into(),
        },
    )?;
    Ok(c)
}

/// The key expressions in a startup list.
pub fn keys(v: Option<&Vec<Json>>) -> Result<Vec<String>> {
    let Some(list) = v else { return Ok(vec![]) };
    ensure!(list.len() <= MAX_KEYS, "at most {MAX_KEYS} key expressions");
    list.iter()
        .map(|k| {
            let k = k.as_str().context("key expressions are strings")?;
            ::zenoh::key_expr::KeyExpr::try_from(k.to_owned())
                .map_err(|e| anyhow::anyhow!("{k:?} is not a key expression: {e}"))?;
            Ok(k.to_owned())
        })
        .collect()
}

pub fn payload_json(bytes: &ZBytes) -> (Json, &'static str) {
    match bytes.try_to_string() {
        Ok(s) => (json!(s), "utf8"),
        Err(_) => (json!(hex::encode(bytes.to_bytes())), "hex"),
    }
}

pub fn sample_json(s: &Sample) -> Json {
    let (payload, enc) = payload_json(s.payload());
    json!({
        "key": s.key_expr().as_str(),
        "kind": if s.kind() == SampleKind::Put { "put" } else { "delete" },
        "payload": payload,
        "payload_encoding": enc,
        "encoding": s.encoding().to_string(),
    })
}

/// An action's payload: text, or hex when `encoding` says so.
pub fn payload_from(v: &Json) -> Result<Vec<u8>> {
    let text = v.get("payload").and_then(Json::as_str).unwrap_or_default();
    let data = match v.get("payload_encoding").and_then(Json::as_str) {
        None | Some("utf8") => text.as_bytes().to_vec(),
        Some("hex") => hex::decode(text).context("payload is not hex")?,
        Some(e) => bail!("payload_encoding {e:?} is utf8 or hex"),
    };
    ensure!(data.len() <= MAX_PAYLOAD, "payload is over 1 MiB");
    Ok(data)
}

fn media(v: &Json) -> Encoding {
    v.get("encoding")
        .and_then(Json::as_str)
        .map(Encoding::from)
        .unwrap_or(Encoding::TEXT_PLAIN)
}

fn key(v: &Json, field: &str) -> Result<String> {
    let k = v
        .get(field)
        .and_then(Json::as_str)
        .with_context(|| format!("{field} is a key expression"))?;
    ::zenoh::key_expr::KeyExpr::try_from(k.to_owned())
        .map_err(|e| anyhow::anyhow!("{k:?} is not a key expression: {e}"))?;
    Ok(k.to_owned())
}

/// Check an action before it runs.
pub fn validate(v: &Json) -> Result<()> {
    match v["type"].as_str() {
        Some("zenoh_put") => {
            key(v, "key")?;
            payload_from(v)?;
        }
        Some("zenoh_delete") => {
            key(v, "key")?;
        }
        Some("zenoh_get") => {
            let s = v
                .get("selector")
                .and_then(Json::as_str)
                .context("selector is a key expression with optional ?parameters")?;
            ::zenoh::key_expr::KeyExpr::try_from(
                s.split('?').next().unwrap_or_default().to_owned(),
            )
            .map_err(|e| anyhow::anyhow!("{s:?} is not a selector: {e}"))?;
        }
        Some("zenoh_reply") => {
            if v.get("key").is_some_and(|k| !k.is_null()) {
                key(v, "key")?;
            }
            payload_from(v)?;
        }
        Some("zenoh_reply_error") => {
            payload_from(v)?;
        }
        _ => bail!("not a Zenoh action"),
    }
    Ok(())
}

/// Run a put, delete or get on the session; a get returns its replies.
pub async fn perform(session: &Session, v: &Json) -> Result<Option<Json>> {
    validate(v)?;
    match v["type"].as_str().unwrap_or_default() {
        "zenoh_put" => {
            session
                .put(key(v, "key")?, payload_from(v)?)
                .encoding(media(v))
                .await
                .map_err(|e| anyhow::anyhow!("put: {e}"))?;
            Ok(None)
        }
        "zenoh_delete" => {
            session
                .delete(key(v, "key")?)
                .await
                .map_err(|e| anyhow::anyhow!("delete: {e}"))?;
            Ok(None)
        }
        "zenoh_get" => {
            let selector = v["selector"].as_str().unwrap_or_default().to_owned();
            let replies = session
                .get(selector.as_str())
                .target(QueryTarget::All)
                .timeout(GET_TIMEOUT)
                .await
                .map_err(|e| anyhow::anyhow!("get: {e}"))?;
            let mut out = Vec::new();
            while let Ok(reply) = replies.recv_async().await {
                if out.len() < MAX_REPLIES {
                    out.push(match reply.result() {
                        Ok(s) => sample_json(s),
                        Err(e) => {
                            let (payload, enc) = payload_json(e.payload());
                            json!({"error": payload, "payload_encoding": enc})
                        }
                    });
                }
            }
            Ok(Some(json!({"selector": selector, "replies": out})))
        }
        other => bail!("{other} is not run on a session"),
    }
}

/// Answer a query with the handler's replies; none at all is an empty answer.
pub async fn answer(query: &Query, actions: &[Json]) -> Result<usize> {
    let mut n = 0;
    for a in actions {
        match a["type"].as_str() {
            Some("zenoh_reply") => {
                let k = match a.get("key").and_then(Json::as_str) {
                    Some(k) => k.to_owned(),
                    None => query.key_expr().as_str().to_owned(),
                };
                query
                    .reply(k, payload_from(a)?)
                    .encoding(media(a))
                    .await
                    .map_err(|e| anyhow::anyhow!("reply: {e}"))?;
                n += 1;
            }
            Some("zenoh_reply_error") => {
                query
                    .reply_err(payload_from(a)?)
                    .await
                    .map_err(|e| anyhow::anyhow!("reply: {e}"))?;
                n += 1;
            }
            _ => {}
        }
    }
    Ok(n)
}

pub fn query_json(q: &Query) -> Json {
    let mut out = json!({
        "selector": q.selector().to_string(),
        "key": q.key_expr().as_str(),
        "parameters": q.parameters().as_str(),
    });
    if let Some(p) = q.payload() {
        let (payload, enc) = payload_json(p);
        out["payload"] = payload;
        out["payload_encoding"] = json!(enc);
    }
    out
}
