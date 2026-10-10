//! WS-Discovery messages (SOAP 1.2 over UDP): the April 2005 version that Windows, ONVIF and
//! wsdd speak, and the OASIS 2009 version, told apart by the discovery namespace a message
//! uses. Shared by NetGet's target service (server) and discovery client.
//!
//! Types are qualified names. The model sees and writes them in Clark notation,
//! `{namespace}LocalName`, which needs no prefix bookkeeping; a few well-known prefixes
//! (`wsdp:`, `pub:`, `dn:`, `tds:`) are accepted on input. On the wire each Types element
//! declares the namespaces its values use, so every value resolves where it stands.
use anyhow::{bail, ensure, Context, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};

pub const PORT: u16 = 3702;
pub const GROUP_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
/// A SOAP-over-UDP message must fit one datagram; NetGet reads at most this much.
pub const MAX_DATAGRAM: usize = 65_507;
/// Element nesting a message may reach; real ones nest about six deep.
pub const MAX_DEPTH: usize = 32;
/// Matches in one ProbeMatches, and types, scopes or addresses in one list.
pub const MAX_ITEMS: usize = 64;

pub fn group() -> SocketAddr {
    SocketAddr::from((GROUP_V4, PORT))
}

const NS_SOAP: &str = "http://www.w3.org/2003/05/soap-envelope";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// WS-Discovery April 2005 (wsdd, Windows, ONVIF).
    V2005,
    /// OASIS WS-Discovery 1.1 (2009).
    V2009,
}

impl Version {
    pub fn discovery_ns(self) -> &'static str {
        match self {
            Version::V2005 => "http://schemas.xmlsoap.org/ws/2005/04/discovery",
            Version::V2009 => "http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01",
        }
    }
    pub fn addressing_ns(self) -> &'static str {
        match self {
            Version::V2005 => "http://schemas.xmlsoap.org/ws/2004/08/addressing",
            Version::V2009 => "http://www.w3.org/2005/08/addressing",
        }
    }
    /// The `To` of a multicast message.
    pub fn to_all(self) -> &'static str {
        match self {
            Version::V2005 => "urn:schemas-xmlsoap-org:ws:2005:04:discovery",
            Version::V2009 => "urn:docs-oasis-open-org:ws-dd:ns:discovery:2009:01",
        }
    }
    /// The `To` of a reply.
    pub fn anonymous(self) -> &'static str {
        match self {
            Version::V2005 => "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous",
            Version::V2009 => "http://www.w3.org/2005/08/addressing/anonymous",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Version::V2005 => "2005/04",
            Version::V2009 => "2009/01",
        }
    }
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "2005/04" | "2005" => Ok(Version::V2005),
            "2009/01" | "2009" | "1.1" => Ok(Version::V2009),
            other => bail!("WS-Discovery version {other:?} is not 2005/04 or 2009/01"),
        }
    }
    fn of(ns: &str) -> Option<Self> {
        [Version::V2005, Version::V2009]
            .into_iter()
            .find(|v| v.discovery_ns() == ns)
    }
}

/// The prefixes these namespaces conventionally carry. Types are written with them on the wire
/// because some implementations compare the Types text literally rather than resolving the
/// QName: wsdd answers `wsdp:Device` and nothing else, whatever the prefix is bound to.
pub const WELL_KNOWN: &[(&str, &str)] = &[
    ("wsdp", "http://schemas.xmlsoap.org/ws/2006/02/devprof"),
    ("pub", "http://schemas.microsoft.com/windows/pub/2005/07"),
    ("dn", "http://www.onvif.org/ver10/network/wsdl"),
    ("tds", "http://www.onvif.org/ver10/device/wsdl"),
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QName {
    pub ns: String,
    pub local: String,
}

impl QName {
    /// `{namespace}Local`, or one of the well-known prefixed forms.
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(rest) = s.strip_prefix('{') {
            let (ns, local) = rest
                .split_once('}')
                .context("a type in Clark notation is {namespace}LocalName")?;
            ensure!(
                !ns.is_empty() && valid_ncname(local),
                "{s:?} is not {{namespace}}LocalName"
            );
            return Ok(QName {
                ns: ns.into(),
                local: local.into(),
            });
        }
        let (prefix, local) = s
            .split_once(':')
            .with_context(|| format!("{s:?} is not a type; write {{namespace}}LocalName"))?;
        let ns = match WELL_KNOWN.iter().find(|(p, _)| *p == prefix) {
            Some((_, ns)) => *ns,
            None => bail!("prefix {prefix:?} is not one NetGet knows (wsdp, pub, dn, tds); write {{namespace}}{local}"),
        };
        ensure!(valid_ncname(local), "{local:?} is not an XML local name");
        Ok(QName {
            ns: ns.into(),
            local: local.into(),
        })
    }
    pub fn clark(&self) -> String {
        format!("{{{}}}{}", self.ns, self.local)
    }
}

fn valid_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_alphabetic() || c == '_')
        && chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// One ProbeMatch, ResolveMatch, Hello or Bye's description of a target service.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Target {
    pub endpoint: String,
    pub types: Vec<QName>,
    pub scopes: Vec<String>,
    pub xaddrs: Vec<String>,
    pub metadata_version: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Probe,
    ProbeMatches,
    Resolve,
    ResolveMatches,
    Hello,
    Bye,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Probe => "Probe",
            Kind::ProbeMatches => "ProbeMatches",
            Kind::Resolve => "Resolve",
            Kind::ResolveMatches => "ResolveMatches",
            Kind::Hello => "Hello",
            Kind::Bye => "Bye",
        }
    }
    fn of(local: &str) -> Option<Self> {
        [
            Kind::Probe,
            Kind::ProbeMatches,
            Kind::Resolve,
            Kind::ResolveMatches,
            Kind::Hello,
            Kind::Bye,
        ]
        .into_iter()
        .find(|k| k.name() == local)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub version: Version,
    pub kind: Kind,
    pub message_id: String,
    pub relates_to: Option<String>,
    /// Probe: the types and scopes asked for, and the scope matching rule.
    pub types: Vec<QName>,
    pub scopes: Vec<String>,
    pub match_by: Option<String>,
    /// Resolve and Bye: the endpoint concerned.
    pub endpoint: Option<String>,
    /// ProbeMatches and ResolveMatches: the matches; Hello: the one service announcing itself.
    pub targets: Vec<Target>,
}

fn local_and_prefix(name: &[u8]) -> (String, Option<String>) {
    let s = String::from_utf8_lossy(name);
    match s.split_once(':') {
        Some((p, l)) => (l.to_string(), Some(p.to_string())),
        None => (s.to_string(), None),
    }
}

/// Parse one datagram. Refuses anything not a SOAP envelope carrying one of the six
/// WS-Discovery messages, nesting past [`MAX_DEPTH`], or lists past [`MAX_ITEMS`].
pub fn parse(datagram: &[u8]) -> Result<Message> {
    ensure!(
        datagram.len() <= MAX_DATAGRAM,
        "a {}-byte datagram exceeds {MAX_DATAGRAM}",
        datagram.len()
    );
    let text = std::str::from_utf8(datagram).context("the message is not UTF-8")?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    // One namespace frame per open element; a stack of (element ns, local name).
    let mut frames: Vec<HashMap<String, String>> = vec![HashMap::new()];
    let mut path: Vec<(String, String)> = Vec::new();
    let mut message_id = None;
    let mut relates_to = None;
    let mut action_body: Option<(Version, Kind)> = None;
    let mut types = Vec::new();
    let mut scopes = Vec::new();
    let mut match_by = None;
    let mut endpoint = None;
    let mut targets: Vec<Target> = Vec::new();
    let mut current: Option<Target> = None;
    let resolve = |frames: &[HashMap<String, String>], prefix: Option<&str>| -> Option<String> {
        let key = prefix.unwrap_or("");
        frames.iter().rev().find_map(|f| f.get(key).cloned())
    };
    loop {
        let event = reader
            .read_event()
            .context("the message is not well-formed XML")?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                ensure!(
                    path.len() < MAX_DEPTH,
                    "elements nest more than {MAX_DEPTH} deep"
                );
                let mut frame = HashMap::new();
                let mut attrs = HashMap::new();
                for a in e.attributes() {
                    let a = a.context("a malformed attribute")?;
                    let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let value = a
                        .decode_and_unescape_value(reader.decoder())
                        .context("a malformed attribute value")?
                        .to_string();
                    if key == "xmlns" {
                        frame.insert(String::new(), value);
                    } else if let Some(p) = key.strip_prefix("xmlns:") {
                        frame.insert(p.to_string(), value);
                    } else {
                        attrs.insert(key, value);
                    }
                }
                frames.push(frame);
                let (local, prefix) = local_and_prefix(e.name().as_ref());
                let ns = resolve(&frames, prefix.as_deref()).unwrap_or_default();
                let empty = matches!(event, Event::Empty(_));
                let in_body =
                    path.len() == 2 && path[1] == (NS_SOAP.to_string(), "Body".to_string());
                if in_body && action_body.is_none() {
                    if let (Some(v), Some(k)) = (Version::of(&ns), Kind::of(&local)) {
                        action_body = Some((v, k));
                    }
                }
                if let Some((v, k)) = action_body {
                    if ns == v.discovery_ns() {
                        let opens_target = matches!(
                            (k, local.as_str()),
                            (Kind::ProbeMatches, "ProbeMatch")
                                | (Kind::ResolveMatches, "ResolveMatch")
                        ) || (path.len() == 2
                            && matches!(k, Kind::Hello | Kind::Bye));
                        if opens_target {
                            ensure!(targets.len() < MAX_ITEMS, "more than {MAX_ITEMS} matches");
                            current = Some(Target::default());
                        }
                        if local == "Scopes" {
                            match_by = attrs.get("MatchBy").cloned();
                        }
                    }
                }
                path.push((ns, local));
                if empty {
                    close(
                        &mut path,
                        &mut frames,
                        action_body,
                        &mut current,
                        &mut targets,
                    );
                }
            }
            Event::Text(t) => {
                let value = t.unescape().context("malformed text")?.trim().to_string();
                let Some((ns, local)) = path.last().cloned() else {
                    continue;
                };
                let parent = path
                    .len()
                    .checked_sub(2)
                    .map(|i| path[i].1.clone())
                    .unwrap_or_default();
                let items = || -> Result<Vec<String>> {
                    let v: Vec<String> = value.split_whitespace().map(str::to_string).collect();
                    ensure!(
                        v.len() <= MAX_ITEMS,
                        "a list of more than {MAX_ITEMS} items"
                    );
                    Ok(v)
                };
                // The header comes before the body, so its version is not known yet: either
                // addressing namespace is accepted there.
                let Some((version, kind)) = action_body else {
                    let addressing = ns == Version::V2005.addressing_ns()
                        || ns == Version::V2009.addressing_ns();
                    if addressing && local == "MessageID" {
                        message_id = Some(value);
                    } else if addressing && local == "RelatesTo" {
                        relates_to = Some(value);
                    }
                    continue;
                };
                if ns == version.discovery_ns() {
                    let slot = current.as_mut();
                    match (local.as_str(), slot) {
                        ("Types", Some(t)) => t.types = qnames(&items()?, &frames)?,
                        ("Types", None) => types = qnames(&items()?, &frames)?,
                        ("Scopes", Some(t)) => t.scopes = items()?,
                        ("Scopes", None) => scopes = items()?,
                        ("XAddrs", Some(t)) => t.xaddrs = items()?,
                        ("MetadataVersion", Some(t)) => t.metadata_version = value.parse().ok(),
                        _ => {}
                    }
                } else if local == "Address"
                    && ns == version.addressing_ns()
                    && parent == "EndpointReference"
                {
                    match current.as_mut() {
                        Some(t) => t.endpoint = value,
                        None if kind == Kind::Resolve => endpoint = Some(value),
                        None => {}
                    }
                }
            }
            Event::End(_) => close(
                &mut path,
                &mut frames,
                action_body,
                &mut current,
                &mut targets,
            ),
            Event::Eof => break,
            _ => {}
        }
    }
    let (version, kind) = action_body.context(
        "not a WS-Discovery message (no Probe, Resolve, Hello, Bye or matches in the body)",
    )?;
    if kind == Kind::Bye {
        endpoint = targets.first().map(|t| t.endpoint.clone());
    }
    Ok(Message {
        version,
        kind,
        message_id: message_id.context("the message has no MessageID")?,
        relates_to,
        types,
        scopes,
        match_by,
        endpoint,
        targets,
    })
}

fn close(
    path: &mut Vec<(String, String)>,
    frames: &mut Vec<HashMap<String, String>>,
    action_body: Option<(Version, Kind)>,
    current: &mut Option<Target>,
    targets: &mut Vec<Target>,
) {
    let closed = path.pop();
    frames.pop();
    if let (Some((ns, local)), Some((v, k))) = (closed, action_body) {
        let closes_target = ns == v.discovery_ns()
            && (matches!(
                (k, local.as_str()),
                (Kind::ProbeMatches, "ProbeMatch") | (Kind::ResolveMatches, "ResolveMatch")
            ) || (path.len() == 2
                && matches!(k, Kind::Hello | Kind::Bye)
                && local == k.name()));
        if closes_target {
            if let Some(t) = current.take() {
                targets.push(t);
            }
        }
    }
}

fn qnames(items: &[String], frames: &[HashMap<String, String>]) -> Result<Vec<QName>> {
    items
        .iter()
        .map(|item| {
            let (prefix, local) = item.split_once(':').unwrap_or(("", item.as_str()));
            let ns = frames
                .iter()
                .rev()
                .find_map(|f| f.get(prefix).cloned())
                .with_context(|| format!("type {item:?} uses an undeclared prefix"))?;
            Ok(QName {
                ns,
                local: local.to_string(),
            })
        })
        .collect()
}

fn esc(s: &str) -> String {
    quick_xml::escape::escape(s).into_owned()
}

/// An envelope with the header every message carries.
fn envelope(
    v: Version,
    action: &str,
    message_id: &str,
    to: &str,
    extra_header: &str,
    body: &str,
) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"{NS_SOAP}\" xmlns:a=\"{a}\" xmlns:d=\"{d}\">\
<s:Header><a:Action>{d}/{action}</a:Action><a:MessageID>{id}</a:MessageID><a:To>{to}</a:To>{extra_header}</s:Header>\
<s:Body>{body}</s:Body></s:Envelope>",
        a = v.addressing_ns(),
        d = v.discovery_ns(),
        id = esc(message_id),
        to = esc(to),
    )
}

fn types_xml(types: &[QName]) -> String {
    if types.is_empty() {
        return String::new();
    }
    let mut declared: Vec<(String, &str)> = Vec::new();
    let mut values = Vec::new();
    for t in types {
        let prefix = match declared.iter().find(|(_, ns)| *ns == t.ns) {
            Some((p, _)) => p.clone(),
            None => {
                let p = WELL_KNOWN
                    .iter()
                    .find(|(_, ns)| *ns == t.ns)
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_else(|| format!("n{}", declared.len()));
                declared.push((p.clone(), &t.ns));
                p
            }
        };
        values.push(format!("{prefix}:{}", t.local));
    }
    let decls: String = declared
        .iter()
        .map(|(p, ns)| format!(" xmlns:{p}=\"{}\"", esc(ns)))
        .collect();
    format!("<d:Types{decls}>{}</d:Types>", values.join(" "))
}

fn list_xml(element: &str, items: &[String], attrs: &str) -> String {
    if items.is_empty() {
        return String::new();
    }
    format!(
        "<d:{element}{attrs}>{}</d:{element}>",
        esc(&items.join(" "))
    )
}

fn target_xml(t: &Target) -> String {
    format!(
        "<a:EndpointReference><a:Address>{}</a:Address></a:EndpointReference>{}{}{}<d:MetadataVersion>{}</d:MetadataVersion>",
        esc(&t.endpoint),
        types_xml(&t.types),
        list_xml("Scopes", &t.scopes, ""),
        list_xml("XAddrs", &t.xaddrs, ""),
        t.metadata_version.unwrap_or(1)
    )
}

fn app_sequence(instance_id: u64, number: u64) -> String {
    format!("<d:AppSequence InstanceId=\"{instance_id}\" MessageNumber=\"{number}\"/>")
}

pub fn new_message_id() -> String {
    format!("urn:uuid:{}", uuid::Uuid::new_v4())
}

/// ProbeMatches or ResolveMatches answering `relates_to`.
pub fn matches(
    v: Version,
    kind: Kind,
    relates_to: &str,
    targets: &[Target],
    instance_id: u64,
    number: u64,
) -> Result<String> {
    let (wrapper, each) = match kind {
        Kind::ProbeMatches => ("ProbeMatches", "ProbeMatch"),
        Kind::ResolveMatches => ("ResolveMatches", "ResolveMatch"),
        other => bail!("{} is not a matches message", other.name()),
    };
    ensure!(targets.len() <= MAX_ITEMS, "at most {MAX_ITEMS} matches");
    let body: String = targets
        .iter()
        .map(|t| format!("<d:{each}>{}</d:{each}>", target_xml(t)))
        .collect();
    let header = format!(
        "<a:RelatesTo>{}</a:RelatesTo>{}",
        esc(relates_to),
        app_sequence(instance_id, number)
    );
    Ok(envelope(
        v,
        wrapper,
        &new_message_id(),
        v.anonymous(),
        &header,
        &format!("<d:{wrapper}>{body}</d:{wrapper}>"),
    ))
}

pub fn probe(
    v: Version,
    message_id: &str,
    types: &[QName],
    scopes: &[String],
    match_by: Option<&str>,
) -> String {
    let attrs = match_by
        .map(|m| format!(" MatchBy=\"{}\"", esc(m)))
        .unwrap_or_default();
    envelope(
        v,
        "Probe",
        message_id,
        v.to_all(),
        "",
        &format!(
            "<d:Probe>{}{}</d:Probe>",
            types_xml(types),
            list_xml("Scopes", scopes, &attrs)
        ),
    )
}

pub fn resolve(v: Version, message_id: &str, endpoint: &str) -> String {
    envelope(
        v,
        "Resolve",
        message_id,
        v.to_all(),
        "",
        &format!("<d:Resolve><a:EndpointReference><a:Address>{}</a:Address></a:EndpointReference></d:Resolve>", esc(endpoint)),
    )
}

pub fn hello(v: Version, t: &Target, instance_id: u64, number: u64) -> String {
    envelope(
        v,
        "Hello",
        &new_message_id(),
        v.to_all(),
        &app_sequence(instance_id, number),
        &format!("<d:Hello>{}</d:Hello>", target_xml(t)),
    )
}

pub fn bye(v: Version, endpoint: &str, instance_id: u64, number: u64) -> String {
    envelope(
        v,
        "Bye",
        &new_message_id(),
        v.to_all(),
        &app_sequence(instance_id, number),
        &format!(
            "<d:Bye><a:EndpointReference><a:Address>{}</a:Address></a:EndpointReference></d:Bye>",
            esc(endpoint)
        ),
    )
}

/// A target as the model writes it.
pub fn target_from_json(v: &serde_json::Value) -> Result<Target> {
    let strings = |key: &str| -> Result<Vec<String>> {
        match v.get(key) {
            None | Some(serde_json::Value::Null) => Ok(vec![]),
            Some(serde_json::Value::Array(a)) => {
                ensure!(
                    a.len() <= MAX_ITEMS,
                    "{key} has more than {MAX_ITEMS} entries"
                );
                a.iter()
                    .map(|x| {
                        let s = x.as_str().with_context(|| format!("{key} holds strings"))?;
                        ensure!(
                            !s.is_empty() && !s.chars().any(char::is_whitespace),
                            "{key} entries are single URIs without spaces: {s:?}"
                        );
                        Ok(s.to_string())
                    })
                    .collect()
            }
            Some(_) => bail!("{key} must be an array of strings"),
        }
    };
    let endpoint = v["endpoint_reference"]
        .as_str()
        .context("endpoint_reference is required, e.g. urn:uuid:...")?;
    ensure!(
        !endpoint.is_empty() && !endpoint.chars().any(char::is_whitespace),
        "endpoint_reference is one URI"
    );
    Ok(Target {
        endpoint: endpoint.into(),
        types: strings("types")?
            .iter()
            .map(|s| QName::parse(s))
            .collect::<Result<_>>()?,
        scopes: strings("scopes")?,
        xaddrs: strings("xaddrs")?,
        metadata_version: match v.get("metadata_version") {
            None | Some(serde_json::Value::Null) => None,
            Some(x) => Some(x.as_u64().context("metadata_version is a whole number")?),
        },
    })
}

pub fn target_json(t: &Target) -> serde_json::Value {
    serde_json::json!({
        "endpoint_reference": crate::utils::sanitize::line_field(&t.endpoint),
        "types": t.types.iter().map(|q| crate::utils::sanitize::line_field(&q.clark())).collect::<Vec<_>>(),
        "scopes": t.scopes.iter().map(|s| crate::utils::sanitize::line_field(s)).collect::<Vec<_>>(),
        "xaddrs": t.xaddrs.iter().map(|s| crate::utils::sanitize::line_field(s)).collect::<Vec<_>>(),
        "metadata_version": t.metadata_version,
    })
}
