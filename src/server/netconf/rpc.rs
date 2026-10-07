//! RFC 6241 envelopes shared by both roles: `<hello>`, `<rpc>` and `<rpc-reply>`.
//!
//! Everything here is a pure transformation between bounded XML documents and structured
//! values. Rust owns message identity (`message-id`, `session-id`), the base-namespace
//! envelope, capability negotiation and the datastore rules a peer can check without asking
//! anyone; a handler decides only what the protocol leaves open — data, acceptance, errors.
use super::xml::{self, Attribute, Document, Node};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};

pub const NC: &str = "urn:ietf:params:xml:ns:netconf:base:1.0";
pub const BASE_10: &str = "urn:ietf:params:netconf:base:1.0";
pub const BASE_11: &str = "urn:ietf:params:netconf:base:1.1";
pub const WRITABLE_RUNNING: &str = "urn:ietf:params:netconf:capability:writable-running:1.0";
pub const CANDIDATE: &str = "urn:ietf:params:netconf:capability:candidate:1.0";
pub const STARTUP: &str = "urn:ietf:params:netconf:capability:startup:1.0";
pub const VALIDATE_10: &str = "urn:ietf:params:netconf:capability:validate:1.0";
pub const VALIDATE_11: &str = "urn:ietf:params:netconf:capability:validate:1.1";
pub const XPATH: &str = "urn:ietf:params:netconf:capability:xpath:1.0";
pub const ROLLBACK: &str = "urn:ietf:params:netconf:capability:rollback-on-error:1.0";

pub const MAX_CAPABILITIES: usize = 256;
pub const MAX_CAPABILITY_BYTES: usize = 1024;
pub const MAX_MESSAGE_ID_BYTES: usize = 256;

/// RFC 6241 Appendix A error tags.
pub const ERROR_TAGS: &[&str] = &[
    "in-use",
    "invalid-value",
    "too-big",
    "missing-attribute",
    "bad-attribute",
    "unknown-attribute",
    "missing-element",
    "bad-element",
    "unknown-element",
    "unknown-namespace",
    "access-denied",
    "lock-denied",
    "resource-denied",
    "rollback-failed",
    "data-exists",
    "data-missing",
    "operation-not-supported",
    "operation-failed",
    "malformed-message",
];
pub const ERROR_TYPES: &[&str] = &["transport", "rpc", "protocol", "application"];

/// Base operations this implementation understands. Anything outside the base namespace is
/// handed to the handler as a custom RPC; base operations not listed here are refused.
pub const BASE_OPERATIONS: &[&str] = &[
    "get",
    "get-config",
    "edit-config",
    "lock",
    "unlock",
    "close-session",
    "kill-session",
    "commit",
    "discard-changes",
    "validate",
];

pub fn capability_ok(uri: &str) -> bool {
    !uri.is_empty()
        && uri.len() <= MAX_CAPABILITY_BYTES
        && uri.is_ascii()
        && !uri.bytes().any(|b| b.is_ascii_whitespace() || b < 0x20 || b == 0x7f)
        && uri.contains(':')
}

fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
fn escape_attribute(text: &str) -> String {
    escape_text(text)
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
        .replace('\t', "&#9;")
}

/// Build a `<hello>`; the server includes its `session-id`, a client must not.
pub fn hello(capabilities: &[String], session_id: Option<u32>) -> Result<Vec<u8>> {
    ensure!(
        !capabilities.is_empty() && capabilities.len() <= MAX_CAPABILITIES,
        "NETCONF capability count"
    );
    let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><hello xmlns=\"{NC}\"><capabilities>");
    for capability in capabilities {
        ensure!(capability_ok(capability), "NETCONF capability URI");
        out.push_str("<capability>");
        out.push_str(&escape_text(capability));
        out.push_str("</capability>");
    }
    out.push_str("</capabilities>");
    if let Some(id) = session_id {
        ensure!(id > 0, "NETCONF session-id must be positive");
        out.push_str(&format!("<session-id>{id}</session-id>"));
    }
    out.push_str("</hello>");
    xml::parse(out.as_bytes())?;
    Ok(out.into_bytes())
}

pub struct Hello {
    pub capabilities: Vec<String>,
    pub session_id: Option<u32>,
}

pub fn parse_hello(message: &[u8]) -> Result<Hello> {
    let doc = xml::parse(message)?;
    ensure!(xml::is_element(&doc, 0, "hello", NC), "NETCONF expected <hello> in the base namespace");
    let mut capabilities = Vec::new();
    let mut session_id = None;
    let mut seen_capabilities = false;
    for child in xml::children(&doc, 0)? {
        if xml::is_element(&doc, child, "capabilities", NC) {
            ensure!(!seen_capabilities, "NETCONF duplicate <capabilities>");
            seen_capabilities = true;
            for capability in xml::children(&doc, child)? {
                ensure!(xml::is_element(&doc, capability, "capability", NC), "NETCONF unexpected element in <capabilities>");
                let uri = xml::text(&doc, capability)?.trim().to_owned();
                ensure!(capability_ok(&uri), "NETCONF capability URI");
                ensure!(capabilities.len() < MAX_CAPABILITIES, "NETCONF capability count");
                if !capabilities.contains(&uri) {
                    capabilities.push(uri);
                }
            }
        } else if xml::is_element(&doc, child, "session-id", NC) {
            ensure!(session_id.is_none(), "NETCONF duplicate <session-id>");
            let text = xml::text(&doc, child)?;
            let id: u32 = text.trim().parse().context("NETCONF session-id must be a 32-bit number")?;
            ensure!(id > 0, "NETCONF session-id must be positive");
            session_id = Some(id);
        }
        // RFC 6241 §8.1: unknown hello content is ignored for extensibility.
    }
    ensure!(seen_capabilities && !capabilities.is_empty(), "NETCONF <hello> without capabilities");
    Ok(Hello { capabilities, session_id })
}

/// Framing both peers agree on, or `None` when they share no base version.
pub fn negotiate(local: &[String], remote: &[String]) -> Option<super::wire::Framing> {
    let both = |v: &str| local.iter().any(|c| c == v) && remote.iter().any(|c| c == v);
    if both(BASE_11) {
        Some(super::wire::Framing::Chunked)
    } else if both(BASE_10) {
        Some(super::wire::Framing::Delimiter)
    } else {
        None
    }
}

/// One `<rpc-error>`, already validated.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub error_type: String,
    pub tag: String,
    pub severity: String,
    pub message: Option<String>,
    pub path: Option<String>,
    pub info: Option<Document>,
}
impl RpcError {
    pub fn new(error_type: &str, tag: &str, message: &str) -> Self {
        Self {
            error_type: error_type.into(),
            tag: tag.into(),
            severity: "error".into(),
            message: Some(message.into()),
            path: None,
            info: None,
        }
    }
    pub fn from_value(v: &Value) -> Result<Self> {
        let obj = v.as_object().context("NETCONF error must be an object")?;
        for key in obj.keys() {
            ensure!(
                matches!(key.as_str(), "error_type" | "error_tag" | "error_severity" | "error_message" | "error_path" | "error_info_xml"),
                "NETCONF error has unknown field '{key}'"
            );
        }
        let s = |k: &str| obj.get(k).map(|v| v.as_str().with_context(|| format!("NETCONF {k} must be a string"))).transpose();
        let error_type = s("error_type")?.unwrap_or("application");
        let tag = s("error_tag")?.context("NETCONF error requires error_tag")?;
        let severity = s("error_severity")?.unwrap_or("error");
        ensure!(ERROR_TYPES.contains(&error_type), "NETCONF error_type must be transport, rpc, protocol or application");
        ensure!(ERROR_TAGS.contains(&tag), "NETCONF error_tag '{tag}' is not an RFC 6241 error tag");
        ensure!(matches!(severity, "error" | "warning"), "NETCONF error_severity must be error or warning");
        let bounded = |v: Option<&str>| -> Result<Option<String>> {
            v.map(|t| {
                ensure!(t.len() <= 4096, "NETCONF error text bound");
                Ok(t.to_owned())
            })
            .transpose()
        };
        let info = s("error_info_xml")?.map(xml::parse_fragment).transpose()?;
        Ok(Self {
            error_type: error_type.into(),
            tag: tag.into(),
            severity: severity.into(),
            message: bounded(s("error_message")?)?,
            path: bounded(s("error_path")?)?,
            info,
        })
    }
    fn render(&self, out: &mut String) -> Result<()> {
        out.push_str("<rpc-error>");
        out.push_str(&format!("<error-type>{}</error-type>", self.error_type));
        out.push_str(&format!("<error-tag>{}</error-tag>", self.tag));
        out.push_str(&format!("<error-severity>{}</error-severity>", self.severity));
        if let Some(path) = &self.path {
            out.push_str(&format!("<error-path>{}</error-path>", escape_text(path)));
        }
        if let Some(message) = &self.message {
            out.push_str(&format!("<error-message xml:lang=\"en\">{}</error-message>", escape_text(message)));
        }
        if let Some(info) = &self.info {
            out.push_str("<error-info>");
            out.push_str(&xml::render_fragment(info)?);
            out.push_str("</error-info>");
        }
        out.push_str("</rpc-error>");
        Ok(())
    }
    pub fn to_value(&self) -> Result<Value> {
        let mut v = json!({"error_type": self.error_type, "error_tag": self.tag, "error_severity": self.severity});
        if let Some(m) = &self.message {
            v["error_message"] = json!(m);
        }
        if let Some(p) = &self.path {
            v["error_path"] = json!(p);
        }
        if let Some(i) = &self.info {
            v["error_info_xml"] = json!(xml::render_fragment(i)?);
        }
        Ok(v)
    }
}

/// What a reply carries besides its identity.
#[derive(Clone, Debug)]
pub enum ReplyBody {
    Ok,
    /// `<data>` for get/get-config.
    Data(Document),
    /// Output elements placed directly under `<rpc-reply>` (custom RPCs).
    Output(Document),
    Errors(Vec<RpcError>),
}

/// Render an `<rpc-reply>`, echoing every attribute of the request's `<rpc>` as RFC 6241
/// §4.2 requires. `attributes` are those of the incoming `<rpc>`, `message-id` among them.
pub fn reply(attributes: &[Attribute], bindings: &[xml::Binding], body: &ReplyBody) -> Result<Vec<u8>> {
    let mut out = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><rpc-reply xmlns=\"{NC}\"");
    for b in bindings {
        // Prefixes the request declared on <rpc> may be used by echoed attributes.
        if b.prefix.is_empty() || b.prefix == "xml" {
            continue;
        }
        out.push_str(&format!(" xmlns:{}=\"{}\"", b.prefix, escape_attribute(&b.uri)));
    }
    for a in attributes {
        out.push_str(&format!(" {}=\"{}\"", a.name, escape_attribute(&a.value)));
    }
    out.push('>');
    match body {
        ReplyBody::Ok => out.push_str("<ok/>"),
        ReplyBody::Data(doc) => {
            out.push_str("<data>");
            out.push_str(&xml::render_fragment(doc)?);
            out.push_str("</data>");
        }
        ReplyBody::Output(doc) => out.push_str(&xml::render_fragment(doc)?),
        ReplyBody::Errors(errors) => {
            ensure!(!errors.is_empty() && errors.len() <= 64, "NETCONF rpc-error count");
            for e in errors {
                e.render(&mut out)?;
            }
        }
    }
    out.push_str("</rpc-reply>");
    ensure!(out.len() <= super::wire::MAX_MESSAGE_BYTES, "NETCONF reply byte bound");
    xml::parse(out.as_bytes()).context("NETCONF rendered reply is not well-formed")?;
    Ok(out.into_bytes())
}

/// Capability-dependent datastore rule shared by both roles.
pub fn datastore_allowed(datastore: &str, capabilities: &[String], writing: bool) -> Result<()> {
    let has = |c: &str| capabilities.iter().any(|v| v == c);
    match datastore {
        "running" => ensure!(!writing || has(WRITABLE_RUNNING), "running is not writable without :writable-running"),
        "candidate" => ensure!(has(CANDIDATE), "candidate datastore requires the :candidate capability"),
        "startup" => ensure!(has(STARTUP), "startup datastore requires the :startup capability"),
        other => bail!("datastore '{other}' is not supported; url and other sources are excluded"),
    }
    Ok(())
}

/// A parsed `<rpc>` as the server sees it.
pub struct IncomingRpc {
    pub message_id: String,
    pub attributes: Vec<Attribute>,
    pub bindings: Vec<xml::Binding>,
    pub operation: String,
    pub namespace: String,
    /// Structured event fields for the handler.
    pub fields: Map<String, Value>,
}

/// Why an incoming message could not become an [`IncomingRpc`]: either it is answerable with
/// an `<rpc-error>` (we know its attributes), or it is not an rpc at all.
pub enum RpcRefusal {
    Reply { attributes: Vec<Attribute>, bindings: Vec<xml::Binding>, error: RpcError },
    Fatal(anyhow::Error),
}

fn datastore(doc: &Document, element: usize) -> Result<String, RpcError> {
    let kids = xml::children(doc, element).map_err(|e| RpcError::new("protocol", "malformed-message", &e.to_string()))?;
    if kids.len() != 1 {
        return Err(RpcError::new("protocol", "missing-element", "exactly one datastore is required"));
    }
    let (name, ns, _) = xml::element(doc, kids[0]).map_err(|e| RpcError::new("protocol", "malformed-message", &e.to_string()))?;
    if ns != NC {
        return Err(RpcError::new("protocol", "unknown-namespace", "datastore must be in the base namespace"));
    }
    Ok(name.to_owned())
}

fn subtree(doc: &Document, element: usize) -> Result<String, RpcError> {
    let inner = xml::inner(doc, element).map_err(|e| RpcError::new("protocol", "malformed-message", &e.to_string()))?;
    xml::render_fragment(&inner).map_err(|e| RpcError::new("application", "too-big", &e.to_string()))
}

/// Parse and check an incoming `<rpc>` against the server's own capabilities.
pub fn parse_rpc(message: &[u8], capabilities: &[String]) -> Result<IncomingRpc, RpcRefusal> {
    let doc = xml::parse(message).map_err(RpcRefusal::Fatal)?;
    let (name, ns, attrs) = xml::element(&doc, 0).map_err(RpcRefusal::Fatal)?;
    if name != "rpc" || ns != NC {
        return Err(RpcRefusal::Fatal(anyhow::anyhow!("NETCONF expected <rpc> in the base namespace")));
    }
    let attributes = attrs.to_vec();
    let bindings = match &doc.nodes[0] {
        Node::Start { bindings, .. } => bindings.clone(),
        _ => Vec::new(),
    };
    let refuse = |error: RpcError| RpcRefusal::Reply { attributes: attributes.clone(), bindings: bindings.clone(), error };
    let message_id = attributes
        .iter()
        .find(|a| a.name == "message-id" && a.namespace.is_empty())
        .map(|a| a.value.clone());
    let Some(message_id) = message_id else {
        let mut error = RpcError::new("rpc", "missing-attribute", "message-id is required");
        error.info = xml::parse_fragment("<bad-attribute>message-id</bad-attribute><bad-element>rpc</bad-element>").ok();
        return Err(refuse(error));
    };
    if message_id.len() > MAX_MESSAGE_ID_BYTES {
        return Err(refuse(RpcError::new("rpc", "bad-attribute", "message-id is too long")));
    }
    let kids = xml::children(&doc, 0).map_err(|e| refuse(RpcError::new("rpc", "malformed-message", &e.to_string())))?;
    if kids.len() != 1 {
        return Err(refuse(RpcError::new("rpc", "malformed-message", "an rpc carries exactly one operation")));
    }
    let op = kids[0];
    let (operation, op_ns, _) = xml::element(&doc, op).map_err(RpcRefusal::Fatal)?;
    let (operation, op_ns) = (operation.to_owned(), op_ns.to_owned());
    let mut fields = Map::new();
    fields.insert("message_id".into(), json!(message_id));
    fields.insert("operation".into(), json!(operation));
    fields.insert("namespace".into(), json!(op_ns));
    if op_ns != NC {
        fields.insert("custom".into(), json!(true));
        let rendered = xml::inner(&doc, op)
            .and_then(|d| xml::render_fragment(&d))
            .map_err(|e| refuse(RpcError::new("application", "too-big", &e.to_string())))?;
        fields.insert("input_xml".into(), json!(rendered));
        return Ok(IncomingRpc { message_id, attributes, bindings, operation, namespace: op_ns, fields });
    }
    if !BASE_OPERATIONS.contains(&operation.as_str()) {
        return Err(refuse(RpcError::new("protocol", "operation-not-supported", "this base operation is not implemented")));
    }
    let children = xml::children(&doc, op).map_err(|e| refuse(RpcError::new("protocol", "malformed-message", &e.to_string())))?;
    let mut seen = std::collections::BTreeSet::new();
    for &child in &children {
        let (child_name, child_ns, child_attrs) = xml::element(&doc, child).map_err(RpcRefusal::Fatal)?;
        if child_ns != NC {
            return Err(refuse(RpcError::new("protocol", "unknown-namespace", "operation parameters must be in the base namespace")));
        }
        if !seen.insert(child_name.to_owned()) {
            return Err(refuse(RpcError::new("protocol", "bad-element", "duplicate operation parameter")));
        }
        let writing = matches!(operation.as_str(), "edit-config" | "commit" | "discard-changes");
        match (operation.as_str(), child_name) {
            ("get-config" | "validate", "source") | ("edit-config" | "lock" | "unlock", "target") => {
                let store = datastore(&doc, child).map_err(refuse)?;
                if let Err(e) = datastore_allowed(&store, capabilities, operation == "edit-config" || (writing && child_name == "target")) {
                    return Err(refuse(RpcError::new("protocol", "invalid-value", &e.to_string())));
                }
                fields.insert(child_name.into(), json!(store));
            }
            ("get" | "get-config", "filter") => {
                let kind = child_attrs.iter().find(|a| a.name == "type").map(|a| a.value.as_str()).unwrap_or("subtree");
                match kind {
                    "subtree" => {
                        fields.insert("filter_type".into(), json!("subtree"));
                        fields.insert("filter_xml".into(), json!(subtree(&doc, child).map_err(refuse)?));
                    }
                    "xpath" if capabilities.iter().any(|c| c == XPATH) => {
                        let select = child_attrs.iter().find(|a| a.name == "select").map(|a| a.value.clone());
                        let Some(select) = select else {
                            return Err(refuse(RpcError::new("protocol", "missing-attribute", "xpath filter requires select")));
                        };
                        fields.insert("filter_type".into(), json!("xpath"));
                        fields.insert("filter_select".into(), json!(select));
                    }
                    _ => return Err(refuse(RpcError::new("protocol", "bad-attribute", "unsupported filter type"))),
                }
            }
            ("edit-config", "config") => {
                fields.insert("config_xml".into(), json!(subtree(&doc, child).map_err(refuse)?));
            }
            ("edit-config", "default-operation") => {
                let v = xml::text(&doc, child).map_err(RpcRefusal::Fatal)?;
                if !matches!(v.trim(), "merge" | "replace" | "none") {
                    return Err(refuse(RpcError::new("protocol", "invalid-value", "default-operation must be merge, replace or none")));
                }
                fields.insert("default_operation".into(), json!(v.trim()));
            }
            ("edit-config", "test-option") => {
                let v = xml::text(&doc, child).map_err(RpcRefusal::Fatal)?;
                let validate = capabilities.iter().any(|c| c == VALIDATE_10 || c == VALIDATE_11);
                if !validate || !matches!(v.trim(), "test-then-set" | "set" | "test-only") {
                    return Err(refuse(RpcError::new("protocol", "invalid-value", "test-option requires :validate and a known value")));
                }
                fields.insert("test_option".into(), json!(v.trim()));
            }
            ("edit-config", "error-option") => {
                let v = xml::text(&doc, child).map_err(RpcRefusal::Fatal)?;
                let ok = match v.trim() {
                    "stop-on-error" | "continue-on-error" => true,
                    "rollback-on-error" => capabilities.iter().any(|c| c == ROLLBACK),
                    _ => false,
                };
                if !ok {
                    return Err(refuse(RpcError::new("protocol", "invalid-value", "unsupported error-option")));
                }
                fields.insert("error_option".into(), json!(v.trim()));
            }
            ("kill-session", "session-id") => {
                let v = xml::text(&doc, child).map_err(RpcRefusal::Fatal)?;
                let id: u32 = v.trim().parse().map_err(|_| refuse(RpcError::new("protocol", "invalid-value", "session-id must be a number")))?;
                fields.insert("session_id".into(), json!(id));
            }
            ("commit", "confirmed" | "confirm-timeout" | "persist" | "persist-id") => {
                return Err(refuse(RpcError::new("protocol", "operation-not-supported", "confirmed commit is not supported")));
            }
            ("validate", "config") => {
                return Err(refuse(RpcError::new("protocol", "operation-not-supported", "validate supports a datastore source only")));
            }
            ("edit-config", "url") | ("get-config" | "validate", "url") => {
                return Err(refuse(RpcError::new("protocol", "operation-not-supported", "url is excluded")));
            }
            _ => return Err(refuse(RpcError::new("protocol", "unknown-element", "unexpected operation parameter"))),
        }
    }
    let need = |key: &str| -> Result<(), RpcRefusal> {
        if fields.contains_key(key) {
            Ok(())
        } else {
            Err(refuse(RpcError::new("protocol", "missing-element", &format!("{key} is required"))))
        }
    };
    match operation.as_str() {
        "get-config" => need("source")?,
        "edit-config" => {
            need("target")?;
            need("config_xml")?;
        }
        "lock" | "unlock" => need("target")?,
        "kill-session" => need("session_id")?,
        "validate" => {
            if !capabilities.iter().any(|c| c == VALIDATE_10 || c == VALIDATE_11) {
                return Err(refuse(RpcError::new("protocol", "operation-not-supported", "validate requires the :validate capability")));
            }
            need("source")?
        }
        "commit" | "discard-changes" if !capabilities.iter().any(|c| c == CANDIDATE) => {
            return Err(refuse(RpcError::new("protocol", "operation-not-supported", "requires the :candidate capability")));
        }
        _ => {}
    }
    Ok(IncomingRpc { message_id, attributes, bindings, operation, namespace: op_ns, fields })
}

/// A client-side request, built from a structured action.
pub fn build_rpc(message_id: u64, action: &Value, server_capabilities: &[String]) -> Result<(String, Vec<u8>)> {
    let operation = action["operation"].as_str().context("NETCONF operation required")?;
    let s = |k: &str| action.get(k).and_then(Value::as_str);
    let mut body = String::new();
    let store = |k: &str, writing: bool, body: &mut String| -> Result<()> {
        let v = s(k).with_context(|| format!("NETCONF {operation} requires {k}"))?;
        datastore_allowed(v, server_capabilities, writing)?;
        body.push_str(&format!("<{k}><{v}/></{k}>"));
        Ok(())
    };
    let filter = |body: &mut String| -> Result<()> {
        if let Some(f) = s("filter_xml") {
            let doc = xml::parse_fragment(f)?;
            body.push_str("<filter type=\"subtree\">");
            body.push_str(&xml::render_fragment(&doc)?);
            body.push_str("</filter>");
        }
        Ok(())
    };
    let op_name = match operation {
        "get" => {
            filter(&mut body)?;
            "get"
        }
        "get-config" => {
            store("source", false, &mut body)?;
            filter(&mut body)?;
            "get-config"
        }
        "edit-config" => {
            store("target", true, &mut body)?;
            if let Some(d) = s("default_operation") {
                ensure!(matches!(d, "merge" | "replace" | "none"), "default_operation must be merge, replace or none");
                body.push_str(&format!("<default-operation>{d}</default-operation>"));
            }
            if let Some(e) = s("error_option") {
                ensure!(matches!(e, "stop-on-error" | "continue-on-error" | "rollback-on-error"), "unknown error_option");
                if e == "rollback-on-error" {
                    ensure!(server_capabilities.iter().any(|c| c == ROLLBACK), "server lacks :rollback-on-error");
                }
                body.push_str(&format!("<error-option>{e}</error-option>"));
            }
            let config = xml::parse_fragment(s("config_xml").context("edit-config requires config_xml")?)?;
            ensure!(!config.nodes.is_empty(), "edit-config requires non-empty config_xml");
            body.push_str("<config>");
            body.push_str(&xml::render_fragment(&config)?);
            body.push_str("</config>");
            "edit-config"
        }
        "lock" | "unlock" => {
            store("target", false, &mut body)?;
            operation
        }
        "commit" | "discard-changes" => {
            ensure!(server_capabilities.iter().any(|c| c == CANDIDATE), "server lacks :candidate");
            operation
        }
        "validate" => {
            ensure!(server_capabilities.iter().any(|c| c == VALIDATE_10 || c == VALIDATE_11), "server lacks :validate");
            store("source", false, &mut body)?;
            "validate"
        }
        "close-session" => "close-session",
        "kill-session" => {
            let id = action["session_id"].as_u64().context("kill-session requires session_id")?;
            ensure!(id > 0 && id <= u64::from(u32::MAX), "session_id out of range");
            body.push_str(&format!("<session-id>{id}</session-id>"));
            "kill-session"
        }
        "custom" => {
            let input = xml::parse_fragment(s("input_xml").context("custom requires input_xml: one operation element with its namespace")?)?;
            let top = input.nodes.iter().filter(|n| matches!(n, Node::Start { .. })).count();
            ensure!(top >= 1, "custom input_xml needs an operation element");
            let rendered = xml::render_fragment(&input)?;
            let doc = xml::parse(format!("<x xmlns=\"urn:netget:probe\">{rendered}</x>").as_bytes())?;
            ensure!(xml::children(&doc, 0)?.len() == 1, "custom input_xml must be exactly one operation element");
            let (_, ns, _) = xml::element(&doc, 1)?;
            ensure!(!ns.is_empty() && ns != NC, "custom operation must be in its own non-base namespace");
            body = rendered;
            ""
        }
        other => bail!("unsupported NETCONF operation '{other}'"),
    };
    let message = if op_name.is_empty() {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><rpc xmlns=\"{NC}\" message-id=\"{message_id}\">{body}</rpc>")
    } else {
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><rpc xmlns=\"{NC}\" message-id=\"{message_id}\"><{op_name}>{body}</{op_name}></rpc>")
    };
    ensure!(message.len() <= super::wire::MAX_MESSAGE_BYTES, "NETCONF request byte bound");
    xml::parse(message.as_bytes())?;
    Ok((operation.to_owned(), message.into_bytes()))
}

/// A client-side view of an `<rpc-reply>`.
pub struct IncomingReply {
    pub message_id: Option<String>,
    pub body: Value,
}

pub fn parse_reply(message: &[u8]) -> Result<IncomingReply> {
    let doc = xml::parse(message)?;
    let (name, ns, attrs) = xml::element(&doc, 0)?;
    ensure!(name == "rpc-reply" && ns == NC, "NETCONF expected <rpc-reply> in the base namespace");
    let message_id = attrs.iter().find(|a| a.name == "message-id" && a.namespace.is_empty()).map(|a| a.value.clone());
    let kids = xml::children(&doc, 0)?;
    let mut ok = false;
    let mut data: Option<String> = None;
    let mut errors = Vec::new();
    let mut output = Vec::new();
    for child in kids {
        let (child_name, child_ns, _) = xml::element(&doc, child)?;
        if child_ns == NC && child_name == "ok" {
            ok = true;
        } else if child_ns == NC && child_name == "data" {
            ensure!(data.is_none(), "NETCONF duplicate <data>");
            data = Some(xml::render_fragment(&xml::inner(&doc, child)?)?);
        } else if child_ns == NC && child_name == "rpc-error" {
            ensure!(errors.len() < 64, "NETCONF rpc-error count");
            let mut e = Map::new();
            for field in xml::children(&doc, child)? {
                let (f, fns, _) = xml::element(&doc, field)?;
                ensure!(fns == NC, "NETCONF rpc-error field namespace");
                let key = match f {
                    "error-type" => "error_type",
                    "error-tag" => "error_tag",
                    "error-severity" => "error_severity",
                    "error-app-tag" => "error_app_tag",
                    "error-path" => "error_path",
                    "error-message" => "error_message",
                    "error-info" => {
                        e.insert("error_info_xml".into(), json!(xml::render_fragment(&xml::inner(&doc, field)?)?));
                        continue;
                    }
                    _ => continue,
                };
                e.insert(key.into(), json!(xml::text(&doc, field)?.trim()));
            }
            ensure!(e.contains_key("error_tag"), "NETCONF rpc-error without error-tag");
            errors.push(Value::Object(e));
        } else {
            let mut piece = xml::inner(&doc, 0)?;
            let end = xml::end_index(&doc, child)?;
            piece.nodes = doc.nodes[child..=end].to_vec();
            output.push(xml::render_fragment(&piece)?);
        }
    }
    let mut body = Map::new();
    if ok {
        body.insert("ok".into(), json!(true));
    }
    if let Some(d) = data {
        body.insert("data_xml".into(), json!(d));
    }
    if !errors.is_empty() {
        body.insert("errors".into(), Value::Array(errors));
    }
    if !output.is_empty() {
        body.insert("output_xml".into(), json!(output.concat()));
    }
    ensure!(!body.is_empty(), "NETCONF <rpc-reply> carries no recognised content");
    Ok(IncomingReply { message_id, body: Value::Object(body) })
}
