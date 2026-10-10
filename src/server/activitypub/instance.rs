//! One small ActivityPub instance, used by both roles: its local actors (each with an RSA
//! key), WebFinger and actor documents, followers and an outbox per actor, fetching and
//! caching remote actors, signed delivery, and verifying what arrives at an inbox. What the
//! actors *say* comes from the model; this is the federation machinery around it.
use super::sig;
use anyhow::{bail, ensure, Context, Result};
use rsa::RsaPrivateKey;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

pub const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";
pub const SECURITY_CONTEXT: &str = "https://w3id.org/security/v1";
pub const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
/// A fetched document or an inbox POST is at most this long.
pub const MAX_DOCUMENT: usize = 1024 * 1024;
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_FOLLOWERS: usize = 10_000;
pub const OUTBOX_KEPT: usize = 50;
pub const MAX_OBJECTS: usize = 1000;
pub const MAX_REMOTE_ACTORS: usize = 1000;
/// Inboxes one post is delivered to at most.
pub const MAX_DELIVERIES: usize = 100;

pub struct LocalActor {
    pub name: String,
    key: RsaPrivateKey,
    pem: String,
    pub followers: BTreeSet<String>,
    /// Remote actors this one follows: id → the Follow activity's id, and whether accepted.
    pub following: HashMap<String, (String, bool)>,
    pub outbox: VecDeque<Value>,
}

#[derive(Clone, Debug)]
pub struct Remote {
    pub id: String,
    pub inbox: String,
    pub shared_inbox: Option<String>,
    pub preferred_username: Option<String>,
    pub name: Option<String>,
    pub key_id: Option<String>,
    pub key_pem: Option<String>,
}

/// An activity that arrived at an inbox with a valid signature.
pub struct Inbound {
    /// The local actor whose inbox it was, or None for the shared inbox.
    pub to: Option<String>,
    pub signer: String,
    pub activity: Value,
}

pub struct Instance {
    /// `http://host:port`, no trailing slash.
    pub base: String,
    /// The host part of handles: `host:port`.
    pub host: String,
    pub actors: Mutex<HashMap<String, LocalActor>>,
    objects: Mutex<(HashMap<String, Value>, VecDeque<String>)>,
    remote: Mutex<HashMap<String, Remote>>,
    proxied: reqwest::Client,
    direct: reqwest::Client,
}

fn loopback(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
                || h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// HTML content as the model reads it: tags dropped, the common entities decoded.
pub fn plain_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for c in html.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                let t = tag.trim_start_matches('/').to_ascii_lowercase();
                if (t.starts_with("br") || t.starts_with('p'))
                    && !out.is_empty()
                    && !out.ends_with('\n')
                {
                    out.push('\n');
                }
            }
            c if in_tag => tag.push(c),
            c => out.push(c),
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

impl Instance {
    pub fn new(base: &str, names: &[String]) -> Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        let host = reqwest::Url::parse(&base)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| match u.port() {
                    Some(p) => format!("{h}:{p}"),
                    None => h.to_string(),
                })
            })
            .context("base_url must be an http(s) URL")?;
        let mut actors = HashMap::new();
        for name in names {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'),
                "actor name {name:?} must be letters, digits, '_', '-' or '.'"
            );
            let key = sig::generate_key()?;
            let pem = sig::public_pem(&key)?;
            actors.insert(
                name.clone(),
                LocalActor {
                    name: name.clone(),
                    key,
                    pem,
                    followers: BTreeSet::new(),
                    following: HashMap::new(),
                    outbox: VecDeque::new(),
                },
            );
        }
        let build = |proxy: bool| {
            let b = reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(3))
                .user_agent(concat!(
                    "NetGet/",
                    env!("CARGO_PKG_VERSION"),
                    " (ActivityPub)"
                ));
            if proxy { b } else { b.no_proxy() }.build()
        };
        Ok(Self {
            base,
            host,
            actors: Mutex::new(actors),
            objects: Mutex::new(Default::default()),
            remote: Mutex::new(HashMap::new()),
            proxied: build(true)?,
            direct: build(false)?,
        })
    }

    fn http(&self, url: &str) -> &reqwest::Client {
        if loopback(url) {
            &self.direct
        } else {
            &self.proxied
        }
    }

    pub fn actor_url(&self, name: &str) -> String {
        format!("{}/users/{name}", self.base)
    }

    pub fn handle(&self, name: &str) -> String {
        format!("{name}@{}", self.host)
    }

    pub fn has_actor(&self, name: &str) -> bool {
        self.actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    pub fn first_actor(&self) -> Option<String> {
        self.actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .min()
            .cloned()
    }

    /// The local actor an `acct:` resource or actor URL names.
    pub fn local_name(&self, s: &str) -> Option<String> {
        let s = s.trim_start_matches("acct:").trim_start_matches('@');
        let name = if let Some(rest) = s.strip_prefix(&format!("{}/users/", self.base)) {
            rest.to_string()
        } else {
            let (user, host) = s.split_once('@')?;
            if !host.eq_ignore_ascii_case(&self.host) {
                return None;
            }
            user.to_string()
        };
        self.has_actor(&name).then_some(name)
    }

    pub fn webfinger(&self, resource: &str) -> Option<Value> {
        let name = self.local_name(resource)?;
        Some(json!({
            "subject": format!("acct:{}", self.handle(&name)),
            "aliases": [self.actor_url(&name)],
            "links": [
                {"rel": "self", "type": "application/activity+json", "href": self.actor_url(&name)},
                {"rel": "http://webfinger.net/rel/profile-page", "type": "text/html", "href": self.actor_url(&name)}
            ]
        }))
    }

    pub fn actor_document(&self, name: &str) -> Option<Value> {
        let actors = self.actors.lock().unwrap_or_else(|e| e.into_inner());
        let a = actors.get(name)?;
        let id = self.actor_url(name);
        Some(json!({
            "@context": [AS_CONTEXT, SECURITY_CONTEXT],
            "id": id,
            "type": "Person",
            "preferredUsername": name,
            "name": name,
            "summary": "<p>An actor served by NetGet</p>",
            "url": id,
            "inbox": format!("{id}/inbox"),
            "outbox": format!("{id}/outbox"),
            "followers": format!("{id}/followers"),
            "following": format!("{id}/following"),
            "endpoints": {"sharedInbox": format!("{}/inbox", self.base)},
            "manuallyApprovesFollowers": true,
            "publicKey": {"id": format!("{id}#main-key"), "owner": id, "publicKeyPem": a.pem}
        }))
    }

    pub fn collection(&self, name: &str, which: &str) -> Option<Value> {
        let actors = self.actors.lock().unwrap_or_else(|e| e.into_inner());
        let a = actors.get(name)?;
        let id = format!("{}/{which}", self.actor_url(name));
        let items: Vec<Value> = match which {
            "followers" => a.followers.iter().map(|f| json!(f)).collect(),
            "following" => a
                .following
                .iter()
                .filter(|(_, (_, ok))| *ok)
                .map(|(f, _)| json!(f))
                .collect(),
            "outbox" => a.outbox.iter().rev().cloned().collect(),
            _ => return None,
        };
        Some(
            json!({"@context": AS_CONTEXT, "id": id, "type": "OrderedCollection",
                    "totalItems": items.len(), "orderedItems": items}),
        )
    }

    pub fn object(&self, id: &str) -> Option<Value> {
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .get(id)
            .cloned()
    }

    fn remember_object(&self, id: &str, v: &Value) {
        let mut o = self.objects.lock().unwrap_or_else(|e| e.into_inner());
        if o.0.insert(id.to_string(), v.clone()).is_none() {
            o.1.push_back(id.to_string());
        }
        while o.1.len() > MAX_OBJECTS {
            if let Some(old) = o.1.pop_front() {
                o.0.remove(&old);
            }
        }
    }

    pub fn new_id(&self, kind: &str) -> String {
        let n: u64 = rand::random();
        format!("{}/{kind}/{n:016x}", self.base)
    }

    // -----------------------------------------------------------------------------------
    // Outgoing HTTP

    fn signed_headers(
        &self,
        from: &str,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<Vec<(String, String)>> {
        let u = reqwest::Url::parse(url).context("not a URL")?;
        let host = match u.port() {
            Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
            None => u.host_str().unwrap_or_default().to_string(),
        };
        let path = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        let mut values = HashMap::new();
        values.insert("host".to_string(), host.clone());
        values.insert("date".to_string(), sig::http_date());
        if let Some(b) = body {
            values.insert("digest".to_string(), sig::digest(b));
        }
        let key = {
            let actors = self.actors.lock().unwrap_or_else(|e| e.into_inner());
            actors.get(from).context("no such local actor")?.key.clone()
        };
        let signature = sig::sign(
            &key,
            &format!("{}#main-key", self.actor_url(from)),
            method,
            &path,
            &values,
        )?;
        let mut out: Vec<(String, String)> = values.into_iter().collect();
        out.push(("signature".into(), signature));
        Ok(out)
    }

    /// GET an ActivityStreams document, signed by `from` when given (for servers that
    /// require authorized fetch).
    pub async fn fetch(&self, url: &str, from: Option<&str>) -> Result<Value> {
        ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "only http(s) URLs can be fetched"
        );
        let mut req = self
            .http(url)
            .get(url)
            .header("accept", "application/activity+json, application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\", application/jrd+json, application/json");
        if let Some(from) = from {
            for (k, v) in self.signed_headers(from, "get", url, None)? {
                if k != "host" {
                    req = req.header(k, v);
                }
            }
        }
        let resp = req.send().await.with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        ensure!(
            resp.content_length().unwrap_or(0) as usize <= MAX_DOCUMENT,
            "{url} is larger than {MAX_DOCUMENT} bytes"
        );
        let mut body = Vec::new();
        let mut resp = resp;
        while let Some(chunk) = resp.chunk().await? {
            body.extend_from_slice(&chunk);
            ensure!(
                body.len() <= MAX_DOCUMENT,
                "{url} is larger than {MAX_DOCUMENT} bytes"
            );
        }
        ensure!(status.is_success(), "GET {url}: {status}");
        serde_json::from_slice(&body).with_context(|| format!("{url} is not JSON"))
    }

    /// A remote actor, fetched once and cached.
    pub async fn remote_actor(&self, id: &str, from: Option<&str>) -> Result<Remote> {
        if let Some(r) = self
            .remote
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
        {
            return Ok(r.clone());
        }
        let doc = self.fetch(id, from).await?;
        let r = Remote {
            id: doc["id"].as_str().unwrap_or(id).to_string(),
            inbox: doc["inbox"]
                .as_str()
                .context("the actor has no inbox")?
                .to_string(),
            shared_inbox: doc["endpoints"]["sharedInbox"].as_str().map(str::to_string),
            preferred_username: doc["preferredUsername"].as_str().map(str::to_string),
            name: doc["name"].as_str().map(str::to_string),
            key_id: doc["publicKey"]["id"].as_str().map(str::to_string),
            key_pem: doc["publicKey"]["publicKeyPem"]
                .as_str()
                .map(str::to_string),
        };
        let mut cache = self.remote.lock().unwrap_or_else(|e| e.into_inner());
        if cache.len() >= MAX_REMOTE_ACTORS {
            cache.clear();
        }
        cache.insert(id.to_string(), r.clone());
        Ok(r)
    }

    /// An actor id from a handle (`user@host`, `@user@host`, `acct:…`) via WebFinger, or a URL.
    pub async fn resolve(&self, target: &str) -> Result<String> {
        if target.starts_with("http://") || target.starts_with("https://") {
            return Ok(target.to_string());
        }
        let acct = target.trim_start_matches("acct:").trim_start_matches('@');
        let (_, host) = acct.split_once('@').context("a handle is user@host")?;
        let scheme = if loopback(&format!("http://{host}/")) {
            "http"
        } else {
            "https"
        };
        let url = format!(
            "{scheme}://{host}/.well-known/webfinger?resource=acct:{}",
            urlencoding::encode(acct)
        );
        let jrd = self.fetch(&url, None).await?;
        jrd["links"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|l| {
                l["rel"] == "self"
                    && l["type"]
                        .as_str()
                        .is_some_and(|t| t.contains("activity+json") || t.contains("ld+json"))
            })
            .and_then(|l| l["href"].as_str())
            .map(str::to_string)
            .with_context(|| format!("WebFinger for {acct} names no ActivityPub actor"))
    }

    /// POST an activity to an inbox, signed by `from`; the HTTP status.
    pub async fn deliver(&self, from: &str, inbox: &str, activity: &Value) -> Result<u16> {
        let body = serde_json::to_vec(activity)?;
        let mut req = self
            .http(inbox)
            .post(inbox)
            .header("content-type", "application/activity+json");
        for (k, v) in self.signed_headers(from, "post", inbox, Some(&body))? {
            if k != "host" {
                req = req.header(k, v);
            }
        }
        let resp = req
            .body(body)
            .send()
            .await
            .with_context(|| format!("POST {inbox}"))?;
        Ok(resp.status().as_u16())
    }

    // -----------------------------------------------------------------------------------
    // What actors do

    pub fn add_follower(&self, name: &str, follower: &str) -> bool {
        let mut actors = self.actors.lock().unwrap_or_else(|e| e.into_inner());
        match actors.get_mut(name) {
            Some(a) if a.followers.len() < MAX_FOLLOWERS || a.followers.contains(follower) => {
                a.followers.insert(follower.to_string());
                true
            }
            _ => false,
        }
    }

    pub fn remove_follower(&self, name: &str, follower: &str) {
        if let Some(a) = self
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(name)
        {
            a.followers.remove(follower);
        }
    }

    pub fn followers(&self, name: &str) -> Vec<String> {
        self.actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|a| a.followers.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Answer a Follow with Accept or Reject, delivered to the follower.
    pub async fn answer_follow(&self, name: &str, follow: &Value, accept: bool) -> Result<u16> {
        let follower = follow["actor"]
            .as_str()
            .context("the Follow has no actor")?
            .to_string();
        if accept {
            ensure!(
                self.add_follower(name, &follower),
                "{name} has too many followers"
            );
        }
        let activity = json!({
            "@context": AS_CONTEXT,
            "id": self.new_id(if accept { "accepts" } else { "rejects" }),
            "type": if accept { "Accept" } else { "Reject" },
            "actor": self.actor_url(name),
            "object": follow,
        });
        let remote = self.remote_actor(&follower, Some(name)).await?;
        self.deliver(name, &remote.inbox, &activity).await
    }

    /// Follow a remote actor; the Follow's id.
    pub async fn follow(&self, name: &str, target: &str) -> Result<(String, u16)> {
        let id = self.resolve(target).await?;
        let remote = self.remote_actor(&id, Some(name)).await?;
        let follow_id = self.new_id("follows");
        let activity = json!({"@context": AS_CONTEXT, "id": follow_id, "type": "Follow",
                              "actor": self.actor_url(name), "object": remote.id});
        if let Some(a) = self
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(name)
        {
            a.following
                .insert(remote.id.clone(), (follow_id.clone(), false));
        }
        let status = self.deliver(name, &remote.inbox, &activity).await?;
        Ok((remote.id, status))
    }

    pub async fn unfollow(&self, name: &str, target: &str) -> Result<(String, u16)> {
        let id = self.resolve(target).await?;
        let follow_id = self
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(name)
            .and_then(|a| a.following.remove(&id))
            .map(|(f, _)| f)
            .with_context(|| format!("{name} does not follow {id}"))?;
        let remote = self.remote_actor(&id, Some(name)).await?;
        let activity = json!({"@context": AS_CONTEXT, "id": self.new_id("undos"), "type": "Undo",
                              "actor": self.actor_url(name),
                              "object": {"id": follow_id, "type": "Follow", "actor": self.actor_url(name), "object": id}});
        Ok((id, self.deliver(name, &remote.inbox, &activity).await?))
    }

    /// Mark a Follow accepted (an Accept arrived for it).
    pub fn follow_answered(&self, name: &str, remote: &str, accepted: bool) {
        if let Some(a) = self
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(name)
        {
            if accepted {
                if let Some(f) = a.following.get_mut(remote) {
                    f.1 = true;
                }
            } else {
                a.following.remove(remote);
            }
        }
    }

    /// Publish a Note as `name`: to its followers and to `to` (actor ids or handles); the
    /// Create activity and each delivery's status.
    pub async fn post(
        &self,
        name: &str,
        content: &str,
        to: &[String],
        public: bool,
        in_reply_to: Option<&str>,
    ) -> Result<(Value, Vec<(String, Result<u16, String>)>)> {
        let me = self.actor_url(name);
        let mut recipients = Vec::new();
        for t in to {
            recipients.push(self.resolve(t).await?);
        }
        let mut audience: Vec<Value> = recipients.iter().map(|r| json!(r)).collect();
        if public {
            audience.insert(0, json!(PUBLIC));
        }
        let note_id = self.new_id("notes");
        let html = content
            .split('\n')
            .map(|l| format!("<p>{}</p>", escape_html(l)))
            .collect::<String>();
        let mut note = json!({
            "id": note_id, "type": "Note", "attributedTo": me, "content": html,
            "published": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "to": audience, "cc": [format!("{me}/followers")],
        });
        if let Some(r) = in_reply_to {
            note["inReplyTo"] = json!(r);
        }
        let create = json!({
            "@context": AS_CONTEXT, "id": format!("{note_id}/activity"), "type": "Create",
            "actor": me, "published": note["published"], "to": note["to"], "cc": note["cc"],
            "object": note,
        });
        self.remember_object(&note_id, &{
            let mut n = create["object"].clone();
            n["@context"] = json!(AS_CONTEXT);
            n
        });
        if let Some(a) = self
            .actors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(name)
        {
            a.outbox.push_back(create.clone());
            while a.outbox.len() > OUTBOX_KEPT {
                a.outbox.pop_front();
            }
        }
        let mut targets: Vec<String> = self.followers(name);
        targets.extend(recipients);
        targets.sort();
        targets.dedup();
        let mut inboxes = Vec::new();
        for t in targets.into_iter().take(MAX_DELIVERIES) {
            match self.remote_actor(&t, Some(name)).await {
                Ok(r) => inboxes.push(r.shared_inbox.unwrap_or(r.inbox)),
                Err(e) => inboxes.push(format!("!{t}: {e:#}")),
            }
        }
        inboxes.sort();
        inboxes.dedup();
        let mut results = Vec::new();
        for inbox in inboxes {
            if let Some(err) = inbox.strip_prefix('!') {
                results.push((err.to_string(), Err("actor not reachable".to_string())));
                continue;
            }
            let r = self
                .deliver(name, &inbox, &create)
                .await
                .map_err(|e| format!("{e:#}"));
            results.push((inbox, r));
        }
        Ok((create, results))
    }

    /// Like an object, delivered to its author (or `to`).
    pub async fn like(&self, name: &str, object: &str, to: &str) -> Result<u16> {
        let target = self.resolve(to).await?;
        let remote = self.remote_actor(&target, Some(name)).await?;
        let activity = json!({"@context": AS_CONTEXT, "id": self.new_id("likes"), "type": "Like",
                              "actor": self.actor_url(name), "object": object});
        self.deliver(name, &remote.inbox, &activity).await
    }

    // -----------------------------------------------------------------------------------
    // Incoming

    /// Verify a POST to an inbox: the signature (with the signer's key, fetched), the
    /// digest, the date, and that the activity's actor is the signer.
    pub async fn verify_inbound(
        &self,
        to: Option<String>,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> Result<Inbound> {
        let header = headers
            .get("signature")
            .context("unsigned: the inbox needs an HTTP Signature")?;
        let parsed = sig::parse(header)?;
        let owner_hint = parsed
            .key_id
            .split('#')
            .next()
            .unwrap_or_default()
            .to_string();
        let (pem, owner) = self
            .key_of(&parsed.key_id, &owner_hint, to.as_deref())
            .await?;
        sig::verify(&parsed, &pem, "post", path, headers, Some(body))?;
        let activity: Value = serde_json::from_slice(body).context("the body is not JSON")?;
        let actor = activity["actor"]
            .as_str()
            .or_else(|| activity["actor"]["id"].as_str())
            .context("the activity has no actor")?;
        if actor != owner {
            bail!("signed by {owner} but the activity's actor is {actor}");
        }
        Ok(Inbound {
            to,
            signer: owner,
            activity,
        })
    }

    /// A key's PEM and owner: the actor document with that key, or a key document.
    async fn key_of(
        &self,
        key_id: &str,
        owner_hint: &str,
        from: Option<&str>,
    ) -> Result<(String, String)> {
        if let Ok(r) = self.remote_actor(owner_hint, from).await {
            if r.key_id.as_deref() == Some(key_id) {
                if let Some(pem) = r.key_pem {
                    return Ok((pem, r.id));
                }
            }
        }
        let doc = self.fetch(key_id, from).await?;
        let key = if doc["publicKeyPem"].is_string() {
            &doc
        } else {
            &doc["publicKey"]
        };
        let pem = key["publicKeyPem"]
            .as_str()
            .context("the key document has no publicKeyPem")?;
        let owner = key["owner"]
            .as_str()
            .or_else(|| doc["id"].as_str())
            .context("the key has no owner")?;
        Ok((pem.to_string(), owner.to_string()))
    }
}

/// What the model is told about an activity.
pub fn summary(a: &Value) -> Value {
    let kind = a["type"].as_str().unwrap_or("?");
    let mut s = json!({"type": kind, "id": a["id"], "actor": a["actor"]});
    let obj = &a["object"];
    match kind {
        "Create" | "Update" => {
            s["object_type"] = obj["type"].clone();
            s["object_id"] = obj["id"].clone();
            if let Some(c) = obj["content"].as_str() {
                s["content"] = json!(plain_text(c));
            }
            s["in_reply_to"] = obj["inReplyTo"].clone();
            s["to"] = obj["to"].clone();
        }
        "Undo" | "Accept" | "Reject" => {
            s["object_type"] = obj["type"].clone();
            s["object_id"] = if obj.is_string() {
                obj.clone()
            } else {
                obj["id"].clone()
            };
        }
        _ => {
            s["object_id"] = if obj.is_string() {
                obj.clone()
            } else {
                obj["id"].clone()
            };
        }
    }
    s
}
