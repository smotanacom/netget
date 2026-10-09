//! RFC 9082 query paths and RFC 9083 response shapes, shared by both roles.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::net::IpAddr;

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_JSON_NODES: usize = 50_000;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_VALUE_BYTES: usize = 255;
pub const MAX_SEARCH_RESULTS: usize = 1000;
pub const CONFORMANCE: &str = "rdap_level_0";
pub const MEDIA_TYPE: &str = "application/rdap+json";
/// Path prefix RDAP queries live under when none is configured.
pub const DEFAULT_BASE_PATH: &str = "/";

/// One parsed RDAP query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    Lookup {
        kind: Lookup,
        value: String,
    },
    Search {
        kind: Search,
        parameter: String,
        value: String,
    },
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    Domain,
    Nameserver,
    Ip,
    Autnum,
    Entity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Search {
    Domains,
    Nameservers,
    Entities,
}

impl Lookup {
    pub fn path(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Nameserver => "nameserver",
            Self::Ip => "ip",
            Self::Autnum => "autnum",
            Self::Entity => "entity",
        }
    }
    /// The `objectClassName` RFC 9083 requires of the answer.
    pub fn class(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Nameserver => "nameserver",
            Self::Ip => "ip network",
            Self::Autnum => "autnum",
            Self::Entity => "entity",
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "domain" => Self::Domain,
            "nameserver" => Self::Nameserver,
            "ip" => Self::Ip,
            "autnum" => Self::Autnum,
            "entity" => Self::Entity,
            _ => return None,
        })
    }
}

impl Search {
    pub fn path(self) -> &'static str {
        match self {
            Self::Domains => "domains",
            Self::Nameservers => "nameservers",
            Self::Entities => "entities",
        }
    }
    pub fn class(self) -> &'static str {
        match self {
            Self::Domains => "domain",
            Self::Nameservers => "nameserver",
            Self::Entities => "entity",
        }
    }
    /// RFC 9083 §8 result member name.
    pub fn member(self) -> &'static str {
        match self {
            Self::Domains => "domainSearchResults",
            Self::Nameservers => "nameserverSearchResults",
            Self::Entities => "entitySearchResults",
        }
    }
    pub fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Domains => &["name", "nsLdhName", "nsIp"],
            Self::Nameservers => &["name", "ip"],
            Self::Entities => &["fn", "handle"],
        }
    }
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "domains" => Self::Domains,
            "nameservers" => Self::Nameservers,
            "entities" => Self::Entities,
            _ => return None,
        })
    }
}

fn percent_decode(text: &str) -> Result<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .context("truncated percent-encoding")?;
            out.push(u8::from_str_radix(hex, 16).context("invalid percent-encoding")?);
            i += 3;
        } else if bytes[i] == b'+' {
            out.push(b'+');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("query value is not UTF-8")
}

pub fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'*' | b':') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn plain(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_VALUE_BYTES,
        "query value must be 1..=255 bytes"
    );
    ensure!(
        !value.chars().any(|c| c.is_control()),
        "query value contains a control character"
    );
    Ok(())
}

/// A domain or nameserver name: labels of letters/digits/hyphens (or U-labels), no empty
/// label, a trailing dot dropped, compared lower-case.
fn name(value: &str, wildcard: bool) -> Result<String> {
    plain(value)?;
    let v = value.strip_suffix('.').unwrap_or(value).to_lowercase();
    ensure!(v.len() <= 253 && !v.is_empty(), "name is too long or empty");
    for (i, label) in v.split('.').enumerate() {
        let star = wildcard && label.contains('*');
        ensure!(
            !label.is_empty() && label.len() <= 63,
            "empty or over-long label"
        );
        ensure!(
            label
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || (star && c == '*')),
            "label '{label}' has a character outside letters, digits and hyphen"
        );
        ensure!(
            !star || i == 0 || label == "*",
            "a wildcard may only stand in the first label"
        );
    }
    Ok(v)
}

fn ip(value: &str) -> Result<String> {
    plain(value)?;
    let (addr, len) = match value.split_once('/') {
        Some((a, l)) => (
            a,
            Some(l.parse::<u8>().context("prefix length must be a number")?),
        ),
        None => (value, None),
    };
    let parsed: IpAddr = addr.parse().context("not an IP address")?;
    let max = if parsed.is_ipv4() { 32 } else { 128 };
    Ok(match len {
        Some(l) => {
            ensure!(l <= max, "prefix length exceeds {max}");
            format!("{parsed}/{l}")
        }
        None => parsed.to_string(),
    })
}

/// Parse a request path (after the base path) and its query string.
pub fn parse(path: &str, query: Option<&str>) -> Result<Query> {
    let path = path.trim_start_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let query_pairs = || -> Result<Vec<(String, String)>> {
        let mut pairs = Vec::new();
        for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            pairs.push((percent_decode(k)?, percent_decode(v)?));
            ensure!(pairs.len() <= 8, "too many query parameters");
        }
        Ok(pairs)
    };
    match segments.as_slice() {
        ["help"] => Ok(Query::Help),
        [kind, rest @ ..] if Lookup::from_name(kind).is_some() && !rest.is_empty() => {
            let kind = Lookup::from_name(kind).expect("checked");
            let raw = match (kind, rest) {
                (Lookup::Ip, [a, l]) => format!("{}/{}", percent_decode(a)?, percent_decode(l)?),
                (_, [v]) => percent_decode(v)?,
                _ => bail!("too many path segments"),
            };
            let value = match kind {
                Lookup::Domain | Lookup::Nameserver => name(&raw, false)?,
                Lookup::Ip => ip(&raw)?,
                Lookup::Autnum => raw
                    .parse::<u32>()
                    .context("autnum must be a 32-bit number")?
                    .to_string(),
                Lookup::Entity => {
                    plain(&raw)?;
                    raw
                }
            };
            Ok(Query::Lookup { kind, value })
        }
        [kind] if Search::from_name(kind).is_some() => {
            let kind = Search::from_name(kind).expect("checked");
            let pairs = query_pairs()?;
            ensure!(pairs.len() == 1, "a search takes exactly one parameter");
            let (parameter, raw) = pairs.into_iter().next().expect("one");
            ensure!(
                kind.parameters().contains(&parameter.as_str()),
                "{} searches take {:?}",
                kind.path(),
                kind.parameters()
            );
            let value = match parameter.as_str() {
                "name" | "nsLdhName" => name(&raw, true)?,
                "nsIp" | "ip" => ip(&raw)?,
                _ => {
                    plain(&raw)?;
                    raw
                }
            };
            Ok(Query::Search {
                kind,
                parameter,
                value,
            })
        }
        _ => bail!("not an RFC 9082 query path"),
    }
}

impl Query {
    /// Event data for the handler.
    pub fn to_event(&self) -> Value {
        match self {
            Query::Lookup { kind, value } => json!({"query_type": kind.path(), "value": value}),
            Query::Search {
                kind,
                parameter,
                value,
            } => json!({"query_type": kind.path(), "search_parameter": parameter, "value": value}),
            Query::Help => json!({"query_type": "help"}),
        }
    }
    /// The relative request path a client sends.
    pub fn path(&self) -> String {
        match self {
            Query::Lookup {
                kind: Lookup::Ip,
                value,
            } => format!("ip/{value}"),
            Query::Lookup { kind, value } => format!("{}/{}", kind.path(), percent_encode(value)),
            Query::Search {
                kind,
                parameter,
                value,
            } => format!("{}?{}={}", kind.path(), parameter, percent_encode(value)),
            Query::Help => "help".into(),
        }
    }
    /// Build from a client action.
    pub fn from_action(v: &Value) -> Result<Self> {
        let kind = v["query_type"].as_str().context("query_type required")?;
        if kind == "help" {
            return Ok(Query::Help);
        }
        let value = v["value"].as_str().context("value required")?;
        if let Some(lookup) = Lookup::from_name(kind) {
            let path = if lookup == Lookup::Ip {
                format!("ip/{value}")
            } else {
                format!("{kind}/{}", percent_encode(value))
            };
            return parse(&path, None);
        }
        let search =
            Search::from_name(kind).with_context(|| format!("unknown query_type '{kind}'"))?;
        let parameter = v["search_parameter"]
            .as_str()
            .unwrap_or(search.parameters()[0]);
        parse(
            search.path(),
            Some(&format!("{parameter}={}", percent_encode(value))),
        )
    }
}

pub fn json_ok(value: &Value) -> bool {
    crate::utils::json_budget::within_budget(value, MAX_BODY_BYTES, MAX_JSON_NODES, MAX_JSON_DEPTH)
}

fn conformance(obj: &mut Map<String, Value>) -> Result<()> {
    let mut list: Vec<Value> = match obj.remove("rdapConformance") {
        None => Vec::new(),
        Some(Value::Array(a)) => a,
        Some(_) => bail!("rdapConformance must be an array of strings"),
    };
    ensure!(
        list.iter().all(Value::is_string) && list.len() <= 64,
        "rdapConformance must be an array of strings"
    );
    if !list.iter().any(|v| v == CONFORMANCE) {
        list.insert(0, json!(CONFORMANCE));
    }
    obj.insert("rdapConformance".into(), Value::Array(list));
    Ok(())
}

fn class_is(obj: &Value, expected: &str) -> Result<()> {
    let got = obj.get("objectClassName").and_then(Value::as_str);
    ensure!(
        got == Some(expected),
        "objectClassName must be \"{expected}\", not {got:?}"
    );
    Ok(())
}

/// What the server sends for a handler's answer.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Body { status: u16, body: Value },
    Redirect(String),
}

pub fn error_body(code: u16, title: &str, description: &[String]) -> Value {
    let mut body = json!({"rdapConformance": [CONFORMANCE], "errorCode": code, "title": title});
    if !description.is_empty() {
        body["description"] = json!(description);
    }
    body
}

/// Turn a validated handler answer into the response for this query.
pub fn answer(query: &Query, action: &Value) -> Result<Answer> {
    let obj = action.as_object().context("response must be an object")?;
    for k in obj.keys() {
        ensure!(
            matches!(
                k.as_str(),
                "type" | "object" | "results" | "not_found" | "error" | "redirect"
            ),
            "unknown response field '{k}'"
        );
    }
    let present: Vec<&str> = ["object", "results", "not_found", "error", "redirect"]
        .into_iter()
        .filter(|k| obj.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
    ensure!(
        present.len() == 1,
        "supply exactly one of object, results, not_found, error, redirect"
    );
    match present[0] {
        "not_found" => {
            ensure!(obj["not_found"] == true, "not_found must be true");
            Ok(Answer::Body {
                status: 404,
                body: error_body(404, "Not Found", &[]),
            })
        }
        "error" => {
            let e = obj["error"]
                .as_object()
                .context("error must be an object")?;
            let code = e
                .get("code")
                .and_then(Value::as_u64)
                .context("error.code required")?;
            ensure!(
                matches!(code, 400 | 403 | 404 | 422 | 429 | 500 | 501 | 503),
                "error.code must be 400, 403, 404, 422, 429, 500, 501 or 503"
            );
            let title = e.get("title").and_then(Value::as_str).unwrap_or("Error");
            ensure!(title.len() <= 256, "error.title too long");
            let description: Vec<String> = match e.get("description") {
                None => Vec::new(),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .context("description entries must be strings")
                    })
                    .collect::<Result<_>>()?,
                Some(Value::String(s)) => vec![s.clone()],
                _ => bail!("error.description must be a string or array of strings"),
            };
            ensure!(
                description.len() <= 16 && description.iter().all(|d| d.len() <= 1024),
                "error.description bound"
            );
            Ok(Answer::Body {
                status: code as u16,
                body: error_body(code as u16, title, &description),
            })
        }
        "redirect" => {
            let url = obj["redirect"]
                .as_str()
                .context("redirect must be a URL string")?;
            ensure!(
                url.len() <= 2048 && (url.starts_with("https://") || url.starts_with("http://")),
                "redirect must be an http(s) URL"
            );
            ensure!(
                !url.chars().any(|c| c.is_whitespace() || c.is_control()),
                "redirect URL has whitespace or controls"
            );
            ensure!(
                matches!(query, Query::Lookup { .. }),
                "only lookups are redirected"
            );
            Ok(Answer::Redirect(url.to_owned()))
        }
        "object" => {
            let mut object = obj["object"].clone();
            ensure!(json_ok(&object), "object exceeds the RDAP response bounds");
            let map = object
                .as_object_mut()
                .context("object must be a JSON object")?;
            match query {
                Query::Lookup { kind, .. } => class_is(&Value::Object(map.clone()), kind.class())?,
                Query::Help => ensure!(
                    map.get("notices").is_some_and(Value::is_array),
                    "a help answer carries notices"
                ),
                Query::Search { .. } => bail!("a search is answered with results, not object"),
            }
            conformance(map)?;
            Ok(Answer::Body {
                status: 200,
                body: object,
            })
        }
        _ => {
            let Query::Search { kind, .. } = query else {
                bail!("results answer searches only")
            };
            let results = obj["results"]
                .as_array()
                .context("results must be an array")?;
            ensure!(
                results.len() <= MAX_SEARCH_RESULTS,
                "too many search results"
            );
            for r in results {
                class_is(r, kind.class())?;
            }
            let mut body = Map::new();
            body.insert(kind.member().into(), Value::Array(results.clone()));
            conformance(&mut body)?;
            let body = Value::Object(body);
            ensure!(json_ok(&body), "results exceed the RDAP response bounds");
            Ok(Answer::Body { status: 200, body })
        }
    }
}

/// Client-side check of a 200 response body against the query that produced it.
pub fn check_response(query: &Query, body: &Value) -> Result<()> {
    ensure!(json_ok(body), "response exceeds the RDAP bounds");
    let obj = body
        .as_object()
        .context("RDAP response must be a JSON object")?;
    let conf = obj
        .get("rdapConformance")
        .and_then(Value::as_array)
        .context("RDAP response lacks rdapConformance")?;
    ensure!(
        conf.iter().all(Value::is_string),
        "rdapConformance must hold strings"
    );
    match query {
        Query::Lookup { kind, .. } => class_is(body, kind.class()),
        Query::Search { kind, .. } => {
            let results = obj
                .get(kind.member())
                .and_then(Value::as_array)
                .with_context(|| format!("search response lacks {}", kind.member()))?;
            for r in results {
                class_is(r, kind.class())?;
            }
            Ok(())
        }
        Query::Help => Ok(()),
    }
}
