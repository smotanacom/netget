//! Selected pull and registry-token semantics, without a content store.
use crate::server::oci_registry::{actions::sha256_digest, is_valid_repository_name};
use anyhow::{bail, ensure, Context, Result};
use hyper::{header, HeaderMap};
use serde_json::{json, Value};
pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const MAX_MANIFEST: usize = 1024 * 1024;
pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 65_536;
pub const MAX_RETAINED: usize = 8 * 1024 * 1024;
pub const MAX_TEXT: usize = 65_536;
pub const MAX_TOKEN: usize = 16_384;
pub const MAX_ITEMS: usize = 1000;
pub const ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.docker.distribution.manifest.list.v2+json";
pub fn within_budget(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_RETAINED, MAX_NODES, MAX_DEPTH)
}
pub fn repository(s: &str) -> Result<()> {
    ensure!(
        is_valid_repository_name(s),
        "OCI repository grammar refusal"
    );
    Ok(())
}
pub fn digest(s: &str) -> Result<()> {
    ensure!(
        s.starts_with("sha256:")
            && s.len() == 71
            && s[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "OCI digest requires lowercase sha256 hex"
    );
    Ok(())
}
pub fn tag(s: &str) -> Result<()> {
    ensure!(
        !s.is_empty() && s.len() <= 128 && s.as_bytes()[0].is_ascii_alphanumeric()
            || s.starts_with('_'),
        "OCI tag refusal"
    );
    ensure!(
        s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
        "OCI tag grammar refusal"
    );
    Ok(())
}
fn text<'a>(v: &'a Value, k: &str, cap: usize) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= cap)
        .context("OCI bounded text field required")
}
pub fn token(s: &str) -> Result<header::HeaderValue> {
    ensure!(
        !s.is_empty()
            && s.len() <= MAX_TOKEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b)),
        "OCI bounded Bearer token refusal"
    );
    let mut h: header::HeaderValue = format!("Bearer {s}").parse()?;
    h.set_sensitive(true);
    Ok(h)
}
#[derive(Clone)]
pub struct Request {
    pub operation: &'static str,
    pub method: &'static str,
    pub path: String,
    pub repository: Option<String>,
    pub reference: Option<String>,
    pub expected_size: Option<u64>,
    pub n: Option<u64>,
    pub last: Option<String>,
}
pub enum Action {
    Request(Request),
    Authenticate {
        username: Option<String>,
        password: Option<String>,
    },
    SetToken(String),
    ClearToken,
    Disconnect,
}
pub fn action(v: &Value) -> Result<Action> {
    ensure!(
        within_budget(v),
        "OCI action depth/node/retained-content limit"
    );
    let kind = text(v, "type", 64)?;
    let mut fields = vec!["type"];
    let result = match kind {
        "disconnect" => Action::Disconnect,
        "oci_clear_token" => Action::ClearToken,
        "oci_set_token" => {
            fields.push("token");
            let s = text(v, "token", MAX_TOKEN)?;
            token(s)?;
            Action::SetToken(s.into())
        }
        "oci_authenticate" => {
            fields.extend(["username", "password"]);
            let username = v
                .get("username")
                .map(|_| text(v, "username", 256))
                .transpose()?;
            let password = v
                .get("password")
                .map(|_| text(v, "password", MAX_TOKEN))
                .transpose()?;
            ensure!(
                username.is_some() == password.is_some(),
                "OCI token basic authentication requires both username and password"
            );
            if let Some(s) = username {
                ensure!(
                    !s.contains(':') && !s.chars().any(char::is_control),
                    "OCI basic principal refusal"
                );
            }
            Action::Authenticate {
                username: username.map(str::to_owned),
                password: password.map(str::to_owned),
            }
        }
        "oci_request" => {
            fields.push("operation");
            let operation = text(v, "operation", 32)?;
            let operation: &'static str = match operation {
                "probe" => "probe",
                "catalog" => "catalog",
                "tags" => "tags",
                "manifest" => "manifest",
                "manifest_head" => "manifest_head",
                "blob" => "blob",
                "blob_head" => "blob_head",
                _ => bail!("unsupported selected OCI operation"),
            };
            let repo = if matches!(operation, "probe" | "catalog") {
                None
            } else {
                fields.push("repository");
                let s = text(v, "repository", 255)?;
                repository(s)?;
                Some(s.to_owned())
            };
            let reference = if matches!(
                operation,
                "manifest" | "manifest_head" | "blob" | "blob_head"
            ) {
                fields.push("reference");
                let s = text(v, "reference", 128)?;
                if s.contains(':') || operation.starts_with("blob") {
                    digest(s)?
                } else {
                    tag(s)?
                }
                Some(s.to_owned())
            } else {
                None
            };
            let mut path = match operation {
                "probe" => "/v2/".into(),
                "catalog" => "/v2/_catalog".into(),
                "tags" => format!("/v2/{}/tags/list", repo.as_ref().unwrap()),
                "manifest" | "manifest_head" => format!(
                    "/v2/{}/manifests/{}",
                    repo.as_ref().unwrap(),
                    reference.as_ref().unwrap()
                ),
                _ => format!(
                    "/v2/{}/blobs/{}",
                    repo.as_ref().unwrap(),
                    reference.as_ref().unwrap()
                ),
            };
            let n = if matches!(operation, "catalog" | "tags") {
                fields.extend(["n", "last"]);
                let n = match v.get("n") {
                    None => 100,
                    Some(v) => v
                        .as_u64()
                        .filter(|n| (1..=MAX_ITEMS as u64).contains(n))
                        .context("OCI page count must be1..1000")?,
                };
                path.push_str(&format!("?n={n}"));
                if v.get("last").is_some() {
                    let last = text(v, "last", 255)?;
                    if operation == "catalog" {
                        repository(last)?
                    } else {
                        tag(last)?
                    }
                    path.push_str("&last=");
                    path.push_str(
                        &url::form_urlencoded::byte_serialize(last.as_bytes()).collect::<String>(),
                    );
                }
                Some(n)
            } else {
                None
            };
            let expected_size = if operation == "blob" {
                fields.push("expected_size");
                v.get("expected_size")
                    .map(|v| {
                        v.as_u64()
                            .filter(|n| *n <= MAX_BODY as u64)
                            .context("OCI expected blob size bound")
                    })
                    .transpose()?
            } else {
                None
            };
            Action::Request(Request {
                operation,
                method: if operation.ends_with("_head") {
                    "HEAD"
                } else {
                    "GET"
                },
                path,
                repository: repo,
                reference,
                expected_size,
                n,
                last: v.get("last").and_then(Value::as_str).map(str::to_owned),
            })
        }
        _ => bail!("unknown selected OCI action"),
    };
    ensure!(
        v.as_object()
            .context("OCI action must be object")?
            .keys()
            .all(|k| fields.contains(&k.as_str())),
        "unsupported selected OCI field"
    );
    Ok(result)
}
pub fn origin(s: &str) -> Result<url::Url> {
    ensure!(s.len() <= 4096, "OCI endpoint bound");
    let u = url::Url::parse(s).map_err(|_| anyhow::anyhow!("invalid OCI origin"))?;
    ensure!(
        matches!(u.scheme(), "http" | "https")
            && u.host().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && matches!(u.path(), "" | "/")
            && u.query().is_none()
            && u.fragment().is_none(),
        "OCI endpoint requires HTTP(S) origin without credentials/path/query"
    );
    #[cfg(target_arch = "wasm32")]
    ensure!(u.scheme() == "http", "browser OCI supports HTTP only");
    Ok(u)
}
pub fn secure_credentials(u: &url::Url) -> bool {
    u.scheme() == "https"
        || match u.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        }
}
fn one<'a>(h: &'a HeaderMap, key: &str) -> Result<Option<&'a str>> {
    let mut values = h.get_all(key).iter();
    let value = values.next();
    ensure!(values.next().is_none(), "duplicate OCI response header");
    value
        .map(|v| v.to_str().context("OCI response header requires text"))
        .transpose()
}
pub fn headers(h: &HeaderMap) -> Result<()> {
    ensure!(
        h.len() <= 64
            && h.iter()
                .map(|(k, v)| k.as_str().len() + v.len())
                .sum::<usize>()
                <= 32768,
        "OCI response header limit"
    );
    ensure!(
        h.values().all(|v| v.len() <= 8192),
        "OCI response header value limit"
    );
    ensure!(
        one(h, "content-encoding")?.is_none_or(|s| s.eq_ignore_ascii_case("identity")),
        "OCI encoded body refused"
    );
    Ok(())
}
fn media(h: &HeaderMap) -> Result<&str> {
    Ok(one(h, "content-type")?
        .context("OCI content type required")?
        .split(';')
        .next()
        .unwrap()
        .trim())
}
fn json_media(h: &HeaderMap) -> Result<()> {
    ensure!(
        media(h)?.eq_ignore_ascii_case("application/json"),
        "OCI JSON content type required"
    );
    Ok(())
}
fn registry_json_media(h: &HeaderMap) -> Result<()> {
    if let Some(mt) = one(h, "content-type")? {
        let mt = mt.split(';').next().unwrap().trim();
        ensure!(
            mt.eq_ignore_ascii_case("application/json") || mt.eq_ignore_ascii_case("text/plain"),
            "OCI registry JSON media refusal"
        );
    }
    Ok(())
}
#[derive(Clone)]
pub struct Challenge {
    pub realm: url::Url,
    pub service: Option<String>,
    pub scopes: Vec<String>,
}
impl Challenge {
    pub fn shown(&self) -> Value {
        json!({"scheme":"Bearer","realm":self.realm.as_str(),"service":self.service,"scopes":self.scopes})
    }
    pub fn token_url(&self) -> url::Url {
        let mut u = self.realm.clone();
        {
            let mut q = u.query_pairs_mut();
            if let Some(s) = &self.service {
                q.append_pair("service", s);
            }
            for s in &self.scopes {
                q.append_pair("scope", s);
            }
            q.append_pair("client_id", "netget");
        }
        u
    }
}
pub fn challenge(h: &HeaderMap, r: &Request) -> Result<Challenge> {
    let s = one(h, "www-authenticate")?.context("OCI401 requires Bearer challenge")?;
    ensure!(s.len() <= 4096, "OCI challenge bound");
    let (scheme, attrs) = s.split_once(' ').context("OCI malformed challenge")?;
    ensure!(
        scheme.eq_ignore_ascii_case("Bearer"),
        "OCI only Bearer challenge is selected"
    );
    let mut attrs = attrs.trim().as_bytes();
    let mut values = std::collections::BTreeMap::new();
    while !attrs.is_empty() {
        let n = attrs
            .iter()
            .position(|b| *b == b'=')
            .context("OCI challenge attribute requires equals")?;
        let name = std::str::from_utf8(&attrs[..n])?
            .trim()
            .to_ascii_lowercase();
        ensure!(
            !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !values.contains_key(&name),
            "OCI challenge attribute refusal"
        );
        attrs = &attrs[n + 1..];
        ensure!(
            attrs.first() == Some(&b'"'),
            "OCI challenge attributes must be quoted"
        );
        attrs = &attrs[1..];
        let mut out = Vec::new();
        let mut closed = false;
        while let Some((&b, rest)) = attrs.split_first() {
            attrs = rest;
            if b == b'"' {
                closed = true;
                break;
            }
            if b == b'\\' {
                let (&b, rest) = attrs
                    .split_first()
                    .context("OCI challenge escape refusal")?;
                ensure!(b == b'"' || b == b'\\', "OCI challenge escape refusal");
                out.push(b);
                attrs = rest;
            } else {
                ensure!(!b.is_ascii_control(), "OCI challenge control refusal");
                out.push(b);
            }
        }
        ensure!(closed, "OCI unterminated challenge");
        values.insert(name, String::from_utf8(out)?);
        while attrs.first().is_some_and(u8::is_ascii_whitespace) {
            attrs = &attrs[1..];
        }
        if attrs.is_empty() {
            break;
        }
        ensure!(attrs[0] == b',', "OCI challenge separator refusal");
        attrs = &attrs[1..];
        while attrs.first().is_some_and(u8::is_ascii_whitespace) {
            attrs = &attrs[1..];
        }
        ensure!(
            !attrs.is_empty() && values.len() < 16,
            "OCI challenge attribute count"
        );
    }
    let realm = url::Url::parse(
        values
            .get("realm")
            .context("OCI challenge realm required")?,
    )
    .map_err(|_| anyhow::anyhow!("invalid OCI token realm"))?;
    ensure!(
        matches!(realm.scheme(), "http" | "https")
            && realm.host().is_some()
            && realm.username().is_empty()
            && realm.password().is_none()
            && realm.fragment().is_none()
            && !realm.query_pairs().any(|(k, _)| matches!(
                k.as_ref(),
                "scope" | "service" | "account" | "client_id" | "offline_token"
            )),
        "OCI token realm refusal"
    );
    let scopes = values
        .get("scope")
        .map(|s| {
            s.split_ascii_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ensure!(scopes.len() <= 16, "OCI scope count");
    for scope in &scopes {
        let expected = if let Some(repo) = &r.repository {
            format!("repository:{repo}:pull")
        } else if r.operation == "catalog" {
            "registry:catalog:*".into()
        } else {
            bail!("OCI probe cannot request repository permissions")
        };
        ensure!(
            *scope == expected,
            "OCI challenge must request only the selected pull scope"
        );
    }
    Ok(Challenge {
        realm,
        service: values.remove("service"),
        scopes,
    })
}
pub fn trusted(c: &Challenge, registry: &url::Url, token_origin: Option<&url::Url>) -> Result<()> {
    ensure!(
        c.realm.origin() == registry.origin()
            || token_origin.is_some_and(|o| c.realm.origin() == o.origin()),
        "OCI token realm origin is not explicitly trusted"
    );
    ensure!(
        secure_credentials(&c.realm),
        "OCI credentials require verified HTTPS or numeric loopback HTTP"
    );
    #[cfg(target_arch = "wasm32")]
    ensure!(
        c.realm.scheme() == "http",
        "browser OCI token transport supports HTTP only"
    );
    Ok(())
}
fn header_digest(h: &HeaderMap) -> Result<Option<&str>> {
    let d = one(h, "docker-content-digest")?;
    if let Some(d) = d {
        digest(d)?;
    }
    Ok(d)
}
fn content_size(h: &HeaderMap) -> Result<Option<u64>> {
    one(h, "content-length")?
        .map(|s| {
            s.parse::<u64>()
                .context("OCI content length requires integer")
        })
        .transpose()
}
fn descriptor(v: &Value) -> Result<()> {
    ensure!(
        v.get("data").is_none(),
        "OCI embedded descriptor bytes are outside the selected scope"
    );
    ensure!(v.is_object(), "OCI descriptor must be object");
    digest(text(v, "digest", 71)?)?;
    text(v, "mediaType", 256)?;
    ensure!(
        v.get("size").and_then(Value::as_u64).is_some(),
        "OCI descriptor size required"
    );
    if let Some(p) = v.get("platform") {
        ensure!(p.is_object(), "OCI platform must be object");
        text(p, "architecture", 256)?;
        text(p, "os", 256)?;
    }
    if let Some(a) = v.get("annotations") {
        ensure!(
            a.as_object()
                .is_some_and(|a| a.values().all(Value::is_string)),
            "OCI annotations must be string map"
        );
    }
    if let Some(u) = v.get("urls") {
        ensure!(
            u.as_array().is_some_and(|u| u.iter().all(Value::is_string)),
            "OCI descriptor URLs must be strings"
        );
    }
    Ok(())
}
fn manifest(v: &Value, mt: &str) -> Result<()> {
    ensure!(
        v.is_object() && v.get("schemaVersion").and_then(Value::as_u64) == Some(2),
        "OCI schemaVersion2 manifest required"
    );
    let index = match mt {
        "application/vnd.oci.image.manifest.v1+json"
        | "application/vnd.docker.distribution.manifest.v2+json" => false,
        "application/vnd.oci.image.index.v1+json"
        | "application/vnd.docker.distribution.manifest.list.v2+json" => true,
        _ => bail!("unsupported selected OCI manifest media type"),
    };
    if let Some(t) = v.get("mediaType") {
        ensure!(
            t.as_str() == Some(mt),
            "OCI document/header media type mismatch"
        );
    }
    if index {
        let a = v
            .get("manifests")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_ITEMS)
            .context("OCI bounded index descriptors required")?;
        for d in a {
            descriptor(d)?;
        }
    } else {
        descriptor(v.get("config").context("OCI config descriptor required")?)?;
        let a = v
            .get("layers")
            .and_then(Value::as_array)
            .filter(|a| a.len() <= MAX_ITEMS)
            .context("OCI bounded layers required")?;
        for d in a {
            descriptor(d)?;
        }
    }
    if let Some(s) = v.get("subject") {
        descriptor(s)?;
    }
    if let Some(a) = v.get("annotations") {
        ensure!(
            a.as_object()
                .is_some_and(|a| a.values().all(Value::is_string)),
            "OCI annotations must be string map"
        );
    }
    if let Some(s) = v.get("artifactType") {
        ensure!(
            s.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 256),
            "OCI artifact type refusal"
        );
    }
    Ok(())
}
pub fn continuation(h: &HeaderMap, r: &Request) -> Result<Option<String>> {
    let Some(s) = one(h, "link")? else {
        return Ok(None);
    };
    ensure!(s.len() <= 4096, "OCI pagination link bound");
    let (target, params) = s.split_once('>').context("OCI pagination link malformed")?;
    let target = target
        .strip_prefix('<')
        .context("OCI pagination angle bracket required")?;
    ensure!(
        target.starts_with('/') && !target.starts_with("//"),
        "OCI pagination requires relative route"
    );
    ensure!(
        params.trim() == "; rel=\"next\"" || params.trim() == "; rel=next",
        "OCI single next pagination link required"
    );
    let base = url::Url::parse("http://netget.invalid")?;
    let next = base.join(target).context("OCI pagination URL refusal")?;
    let expected = r.path.split('?').next().unwrap();
    ensure!(
        next.origin() == base.origin()
            && next.path() == expected
            && next.username().is_empty()
            && next.password().is_none()
            && next.fragment().is_none(),
        "OCI pagination must stay on selected relative route"
    );
    let mut n = None;
    let mut last = None;
    for (k, v) in next.query_pairs() {
        match k.as_ref() {
            "n" => {
                ensure!(n.is_none(), "OCI duplicate pagination parameter");
                n = Some(v.parse::<u64>()?);
            }
            "last" => {
                ensure!(last.is_none(), "OCI duplicate pagination parameter");
                last = Some(v.into_owned());
            }
            _ => bail!("OCI pagination parameter refusal"),
        }
    }
    ensure!(
        (n.is_none() || n == r.n) && last.as_ref().is_some_and(|s| !s.is_empty()),
        "OCI pagination count/cursor refusal"
    );
    let last = last.unwrap();
    if r.operation == "catalog" {
        repository(&last)?
    } else {
        tag(&last)?
    }
    Ok(Some(last))
}
pub enum Outcome {
    Result(Value),
    Challenge(Challenge, Value),
    Failure(Value),
}
pub fn response(r: &Request, status: u16, h: &HeaderMap, body: &[u8]) -> Result<Outcome> {
    headers(h)?;
    ensure!(body.len() <= MAX_BODY, "OCI response body bound");
    if status == 401 {
        let c = challenge(h, r)?;
        return Ok(Outcome::Challenge(
            c,
            json!({"operation":r.operation,"status":status,"errors":errors(h,body)?}),
        ));
    }
    if status != 200 {
        ensure!(
            (400..=599).contains(&status),
            "OCI redirects/nonstandard success refused"
        );
        return Ok(Outcome::Failure(
            json!({"operation":r.operation,"status":status,"errors":errors(h,body)?,"retry_after":one(h,"retry-after")?}),
        ));
    }
    if let Some(s) = one(h, "docker-distribution-api-version")? {
        ensure!(s == "registry/2.0", "OCI API version header refusal");
    }
    let data = match r.operation {
        "probe" => json!({"api_available":true}),
        "catalog" | "tags" => {
            registry_json_media(h)?;
            let v = json_body(body)?;
            let key = if r.operation == "catalog" {
                "repositories"
            } else {
                "tags"
            };
            if r.operation == "tags" {
                ensure!(
                    v.get("name").and_then(Value::as_str) == r.repository.as_deref(),
                    "OCI tags repository mismatch"
                );
            }
            let items = match v.get(key) {
                Some(Value::Array(a)) => a.as_slice(),
                Some(Value::Null) if r.operation == "tags" => &[],
                _ => bail!("OCI response list required"),
            };
            ensure!(
                items.len() <= r.n.unwrap() as usize,
                "OCI response page count refusal"
            );
            let mut previous = r.last.as_deref();
            let mut seen = std::collections::HashSet::new();
            for x in items {
                let s = x.as_str().context("OCI list entry requires string")?;
                if key == "tags" {
                    tag(s)?
                } else {
                    repository(s)?
                }
                ensure!(seen.insert(s), "OCI duplicate list entry");
                if let Some(p) = previous {
                    let ordered = if key == "tags" {
                        p.to_ascii_lowercase() <= s.to_ascii_lowercase()
                    } else {
                        p < s
                    };
                    ensure!(ordered && p != s, "OCI list ordering/cursor refusal");
                }
                previous = Some(s);
            }
            let mut next = continuation(h, r)?;
            let mut continuation_source = if next.is_some() { "link" } else { "none" };
            if let Some(last) = &next {
                ensure!(
                    items.last().and_then(Value::as_str) == Some(last.as_str()),
                    "OCI pagination cursor must match last returned entry"
                );
            }
            // A registry MAY omit Link even when further tags exist. A full page
            // therefore needs another explicit cursor request to establish completion.
            if next.is_none() && items.len() == r.n.unwrap() as usize {
                next = items.last().and_then(Value::as_str).map(str::to_owned);
                continuation_source = "count_fallback";
            }
            json!({key:items,"repository":r.repository,"next_last":next,"pagination_complete":next.is_none(),"continuation_source":continuation_source})
        }
        "manifest" => {
            ensure!(body.len() <= MAX_MANIFEST, "OCI manifest byte cap");
            let mt = media(h)?;
            let d = sha256_digest(body);
            if let Some(expected) = r.reference.as_deref().filter(|s| s.contains(':')) {
                ensure!(d == expected, "OCI manifest digest mismatch");
            }
            if let Some(expected) = header_digest(h)? {
                ensure!(d == expected, "OCI manifest header digest mismatch");
            }
            let v = json_body(body)?;
            manifest(&v, mt)?;
            json!({"manifest":v,"digest":d,"digest_verified":true,"size":body.len(),"media_type":mt})
        }
        "blob" => {
            let d = sha256_digest(body);
            ensure!(
                r.reference.as_deref() == Some(d.as_str()),
                "OCI blob digest mismatch"
            );
            if let Some(expected) = header_digest(h)? {
                ensure!(d == expected, "OCI blob header digest mismatch");
            }
            if let Some(n) = r.expected_size {
                ensure!(n == body.len() as u64, "OCI blob descriptor size mismatch");
            }
            let text = std::str::from_utf8(body)
                .ok()
                .filter(|s| s.len() <= MAX_TEXT);
            json!({"digest":d,"digest_verified":true,"size":body.len(),"media_type":one(h,"content-type")?,"text":text,"content_omitted":text.is_none()})
        }
        "manifest_head" | "blob_head" => {
            ensure!(body.is_empty(), "OCI HEAD must have no body");
            let d = header_digest(h)?;
            if let (Some(expected), Some(actual)) =
                (r.reference.as_deref().filter(|s| s.contains(':')), d)
            {
                ensure!(expected == actual, "OCI HEAD digest mismatch");
            }
            json!({"digest":d,"digest_verified":false,"size":content_size(h)?,"media_type":one(h,"content-type")?,"exists":true})
        }
        _ => bail!("unsupported OCI response operation"),
    };
    Ok(Outcome::Result(
        json!({"operation":r.operation,"status":status,"repository":r.repository,"reference":r.reference,"data":data}),
    ))
}
fn errors(h: &HeaderMap, body: &[u8]) -> Result<Value> {
    if body.is_empty() {
        return Ok(Value::Null);
    }
    registry_json_media(h)?;
    let v = json_body(body)?;
    let errors = v
        .get("errors")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= 64)
        .context("OCI bounded error envelope required")?;
    for e in errors {
        let code = text(e, "code", 128)?;
        ensure!(
            code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
            "OCI error code refusal"
        );
        ensure!(
            e.get("message").and_then(Value::as_str).is_some(),
            "OCI error message required"
        );
    }
    Ok(Value::Array(errors.clone()))
}
pub struct IssuedToken {
    pub secret: String,
    pub ttl: std::time::Duration,
    pub metadata: Value,
}
pub fn issued_token(status: u16, h: &HeaderMap, body: &[u8]) -> Result<IssuedToken> {
    headers(h)?;
    ensure!(status == 200, "OCI token service refused authentication");
    json_media(h)?;
    let v = json_body(body)?;
    let a = v
        .get("token")
        .map(|_| text(&v, "token", MAX_TOKEN))
        .transpose()?;
    let b = v
        .get("access_token")
        .map(|_| text(&v, "access_token", MAX_TOKEN))
        .transpose()?;
    ensure!(
        a.is_none() || b.is_none() || a == b,
        "OCI token aliases disagree"
    );
    let s = a.or(b).context("OCI token service omitted token")?;
    token(s)?;
    let seconds = match v.get("expires_in") {
        None => 60,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=86400).contains(n))
            .context("OCI token expiry bound")?,
    };
    let remaining = if let Some(t) = v.get("issued_at") {
        let t = t.as_str().context("OCI token issue time requires text")?;
        let t = chrono::DateTime::parse_from_rfc3339(t)
            .context("OCI token issue time requires RFC3339")?;
        let deadline = t.with_timezone(&chrono::Utc) + chrono::Duration::seconds(seconds as i64);
        let now = chrono::Utc::now();
        ensure!(
            t <= now + chrono::Duration::seconds(30),
            "OCI token issue time is in the future"
        );
        let ms = (deadline - now).num_milliseconds();
        ensure!(ms > 0, "OCI issued token already expired");
        std::time::Duration::from_millis(ms as u64).min(std::time::Duration::from_secs(seconds))
    } else {
        std::time::Duration::from_secs(seconds)
    };
    Ok(IssuedToken {
        secret: s.into(),
        ttl: remaining,
        metadata: json!({"token_received":true,"registry_authorization_verified":false,"expires_in":v.get("expires_in"),"effective_ttl_ms":remaining.as_millis() as u64,"issued_at":v.get("issued_at")}),
    })
}
pub fn redact(v: &mut Value, secrets: &[String]) {
    redact_inner(v, secrets, 0)
}
fn redact_inner(v: &mut Value, secrets: &[String], depth: usize) {
    if depth > MAX_DEPTH + 8 {
        let old = std::mem::replace(v, Value::Null);
        crate::utils::json_budget::drop_iteratively(old);
        return;
    }
    match v {
        Value::String(s) => {
            if secrets.iter().filter(|s| !s.is_empty()).any(|secret| {
                let e = serde_json::to_string(secret).unwrap();
                let debug = format!("{secret:?}");
                s.contains(secret)
                    || s.contains(&e[1..e.len() - 1])
                    || s.contains(&debug[1..debug.len() - 1])
            }) {
                *s = crate::utils::redact::REDACTED.into();
            }
        }
        Value::Array(a) => {
            for v in a {
                redact_inner(v, secrets, depth + 1)
            }
        }
        Value::Object(m) => {
            let old = std::mem::take(m);
            for (k, mut v) in old {
                let mut key = Value::String(k);
                redact_inner(&mut key, secrets, depth + 1);
                redact_inner(&mut v, secrets, depth + 1);
                m.insert(key.as_str().unwrap().into(), v);
            }
        }
        _ => {}
    }
}

struct Seed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
impl<'de> serde::de::DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        use serde::de::Error;
        *self.nodes += 1;
        if self.depth > MAX_DEPTH || *self.nodes > MAX_NODES {
            return Err(D::Error::custom("OCI JSON nesting/node limit"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded OCI JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("OCI text limit"));
        }
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("OCI text limit"));
        }
        Ok(Value::String(v))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = Vec::new();
        while let Some(v) = a.next_element_seed(Seed {
            depth: self.depth + 1,
            nodes: self.nodes,
        })? {
            if values.len() >= MAX_ITEMS {
                return Err(A::Error::custom("OCI array limit"));
            }
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = serde_json::Map::new();
        while let Some(key) = a.next_key::<String>()? {
            if key.len() > 256 || values.len() >= 256 {
                return Err(A::Error::custom("OCI object field/name limit"));
            }
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate OCI JSON field"));
            }
            let value = a.next_value_seed(Seed {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
pub fn json_body(body: &[u8]) -> Result<Value> {
    ensure!(body.len() <= MAX_BODY, "OCI body limit");
    use serde::de::DeserializeSeed;
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let value = Seed {
        depth: 0,
        nodes: &mut 0,
    }
    .deserialize(&mut decoder)
    .context("invalid OCI JSON")?;
    decoder.end().context("trailing OCI JSON content")?;
    Ok(value)
}
#[derive(Clone, Copy)]
enum Schema {
    Envelope,
    Manifest,
    Descriptor,
    Platform,
    Error,
    Generic,
}
pub fn redact_payload(v: &mut Value, secrets: &[String]) {
    redact_schema(v, secrets, 0, Schema::Envelope);
}
fn redact_schema(v: &mut Value, secrets: &[String], depth: usize, schema: Schema) {
    if depth > MAX_DEPTH + 8 {
        let old = std::mem::replace(v, Value::Null);
        crate::utils::json_budget::drop_iteratively(old);
        return;
    }
    match v {
        Value::Array(a) => {
            for v in a {
                redact_schema(v, secrets, depth + 1, schema)
            }
        }
        Value::Object(m) => {
            let old = std::mem::take(m);
            for (k, mut v) in old {
                let fixed = match schema {
                    Schema::Envelope => matches!(
                        k.as_str(),
                        "operation"
                            | "status"
                            | "repository"
                            | "reference"
                            | "data"
                            | "manifest"
                            | "digest"
                            | "digest_verified"
                            | "size"
                            | "media_type"
                            | "text"
                            | "content_omitted"
                            | "exists"
                            | "repositories"
                            | "tags"
                            | "next_last"
                            | "pagination_complete"
                            | "continuation_source"
                            | "api_available"
                            | "errors"
                            | "retry_after"
                            | "challenge"
                            | "scheme"
                            | "realm"
                            | "service"
                            | "scopes"
                            | "origin"
                            | "token_present"
                            | "authentication_verified"
                            | "category"
                            | "error"
                            | "token_received"
                            | "registry_authorization_verified"
                            | "expires_in"
                            | "effective_ttl_ms"
                            | "issued_at"
                    ),
                    Schema::Manifest => matches!(
                        k.as_str(),
                        "schemaVersion"
                            | "mediaType"
                            | "config"
                            | "layers"
                            | "manifests"
                            | "subject"
                            | "artifactType"
                            | "annotations"
                    ),
                    Schema::Descriptor => matches!(
                        k.as_str(),
                        "mediaType"
                            | "digest"
                            | "size"
                            | "urls"
                            | "annotations"
                            | "platform"
                            | "artifactType"
                            | "data"
                    ),
                    Schema::Platform => matches!(
                        k.as_str(),
                        "architecture"
                            | "os"
                            | "variant"
                            | "os.version"
                            | "os.features"
                            | "features"
                    ),
                    Schema::Error => matches!(k.as_str(), "code" | "message" | "detail"),
                    Schema::Generic => false,
                };
                let next = match (schema, k.as_str()) {
                    (Schema::Envelope, "data" | "challenge") => Schema::Envelope,
                    (Schema::Envelope, "manifest") => Schema::Manifest,
                    (Schema::Envelope, "errors") => Schema::Error,
                    (Schema::Manifest, "config" | "layers" | "manifests" | "subject") => {
                        Schema::Descriptor
                    }
                    (Schema::Descriptor, "platform") => Schema::Platform,
                    _ => Schema::Generic,
                };
                let key = if fixed {
                    k
                } else {
                    let mut key = Value::String(k);
                    redact(&mut key, secrets);
                    key.as_str().unwrap().into()
                };
                redact_schema(&mut v, secrets, depth + 1, next);
                m.insert(key, v);
            }
        }
        _ => redact(v, secrets),
    }
}
