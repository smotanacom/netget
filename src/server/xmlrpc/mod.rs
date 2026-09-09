//! XML-RPC server implementation
//!
//! This module implements an XML-RPC server over HTTP that allows LLM control
//! over RPC method execution, introspection, and response generation.
//!
//! XML-RPC specification: http://xmlrpc.com/spec.md
//!
//! The LLM controls:
//! - Method execution (custom methods defined by user prompt)
//! - Introspection responses (system.listMethods, system.methodHelp, etc.)
//! - Fault generation for errors
//! - Extensions (nil values, i8/64-bit integers, system.multicall)

pub mod actions;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event as XmlEvent};
use quick_xml::{Reader, Writer};
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ActionResult;
#[cfg(feature = "xmlrpc")]
use crate::logging::emit::Log;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::server::{
    ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo, ServerId,
};

pub use actions::XmlRpcProtocol;

/// Largest request body accepted, in bytes. hyper imposes no limit of its own, and
/// the body is buffered whole, copied into a `String` and traced in full.
#[cfg(feature = "xmlrpc")]
const MAX_REQUEST_BODY_BYTES: usize = 4 * 1024 * 1024;

/// XML-RPC fault codes used for netget's own failures.
///
/// The peer gets a category, never the error: an LLM backend URL, a model name or an
/// `anyhow` context chain is for the log, not for a stranger's client. See
/// `src/utils/wire_failure.rs`.
///
/// `-32000..=-32099` is the implementation-defined server-error range in the XML-RPC
/// fault-code interop note, so a saturated backend gets its own retryable code instead of
/// being flattened into the permanent `-32603 internal error`.
#[cfg(feature = "xmlrpc")]
const FAULT_INTERNAL_ERROR: i32 = -32603;
/// Backend saturated — transient, the client should back off and retry.
#[cfg(feature = "xmlrpc")]
const FAULT_SERVER_BUSY: i32 = -32000;

/// XML-RPC server that handles RPC method calls with LLM
pub struct XmlRpcServer;

#[cfg(feature = "xmlrpc")]
impl XmlRpcServer {
    /// Spawn XML-RPC server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("XML-RPC server listening on {}", local_addr));

        let protocol = Arc::new(XmlRpcProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        Log::new(Some(&status_tx)).info(format!(
                            "XML-RPC connection {} from {}",
                            connection_id, remote_addr
                        ));

                        // Track connection in server state
                        let local_addr_conn = stream.local_addr().unwrap_or(listen_addr);
                        let now = std::time::Instant::now();

                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };

                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        // Spawn connection handler
                        tokio::spawn(async move {
                            let io = hyper_util::rt::TokioIo::new(stream);

                            // Create service function for this connection
                            let service = hyper::service::service_fn(|req| {
                                handle_xmlrpc_request(
                                    req,
                                    connection_id,
                                    server_id,
                                    remote_addr,
                                    llm_clone.clone(),
                                    state_clone.clone(),
                                    status_clone.clone(),
                                    protocol_clone.clone(),
                                )
                            });

                            // Serve HTTP/1 connection
                            if let Err(e) = hyper::server::conn::http1::Builder::new()
                                .serve_connection(io, service)
                                .await
                            {
                                Log::new(Some(&status_clone)).error(format!(
                                    "XML-RPC connection {} error: {}",
                                    connection_id, e
                                ));
                            }

                            // Mark connection as closed
                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_clone))
                                .info(format!("XML-RPC connection {} closed", connection_id));
                            let _ = status_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept XML-RPC connection: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Handle a single XML-RPC request
#[cfg(feature = "xmlrpc")]
async fn handle_xmlrpc_request(
    req: Request<hyper::body::Incoming>,
    connection_id: ConnectionId,
    server_id: ServerId,
    remote_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<XmlRpcProtocol>,
) -> Result<Response<Full<Bytes>>> {
    // Collect request body, capped: hyper imposes no limit, and the body was
    // buffered whole, copied into a String and then traced in full.
    let (parts, body) = req.into_parts();
    let body_bytes = http_body_util::Limited::new(body, MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read request body: {}", e))?
        .to_bytes();

    let body_str = String::from_utf8_lossy(&body_bytes);

    let log = Log::new(Some(&status_tx));
    log.debug(format!(
        "XML-RPC request: {} {} ({} bytes)",
        parts.method,
        parts.uri,
        body_bytes.len()
    ));

    // Full request body, file only.
    log.trace(format!("XML-RPC request body:\n{}", body_str));

    // Check if it's POST (XML-RPC requires POST)
    if parts.method != hyper::Method::POST {
        let fault_xml = generate_fault(-32600, "Invalid request: XML-RPC requires POST method");
        Log::new(Some(&status_tx)).debug(format!(
            "XML-RPC error: invalid method {} (expected POST)",
            parts.method
        ));
        return Ok(Response::builder()
            .status(200)
            .header("Content-Type", "text/xml; charset=utf-8")
            .body(Full::new(Bytes::from(fault_xml)))
            .unwrap());
    }

    // Parse XML-RPC methodCall
    let method_call = match parse_method_call(&body_str) {
        Ok(call) => call,
        Err(e) => {
            tracing::warn!("XML-RPC parse error (decision=reject_malformed_request): {e:#}");
            Log::new(Some(&status_tx)).warn(format!("XML-RPC parse error: {}", e));
            // The codec's message names library internals; the peer learns only that its
            // document did not parse.
            let fault_xml = generate_fault(-32700, "Parse error: malformed XML-RPC methodCall");
            return Ok(Response::builder()
                .status(200)
                .header("Content-Type", "text/xml; charset=utf-8")
                .body(Full::new(Bytes::from(fault_xml)))
                .unwrap());
        }
    };

    Log::new(Some(&status_tx)).debug(format!(
        "XML-RPC method: {} ({} params)",
        method_call.method_name,
        method_call.params.len()
    ));

    // Create event for LLM
    let event = actions::create_method_call_event(&method_call);

    // Call LLM to get response
    let execution_result = match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        protocol.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            // Three cases must stay apart in the log: the model rejected (it emits its own
            // `xmlrpc_fault_response`, so it never reaches here), the model answered nothing
            // (below), and the LLM call itself errored (here).
            let failure = crate::utils::WireFailure::classify(&e);
            let (fault_code, decision) = if failure.is_overloaded() {
                (FAULT_SERVER_BUSY, "fail_closed_llm_overloaded")
            } else {
                (FAULT_INTERNAL_ERROR, "fail_closed_llm_error")
            };
            tracing::error!(
                "XML-RPC LLM error (decision={decision}, fault_code={fault_code}): {e:#}"
            );
            Log::new(Some(&status_tx))
                .warn(format!("XML-RPC LLM error (decision={}): {}", decision, e));
            // Category only — never the error text.
            let fault_xml = generate_fault(fault_code, failure.text());
            return Ok(Response::builder()
                .status(200)
                .header("Content-Type", "text/xml; charset=utf-8")
                .body(Full::new(Bytes::from(fault_xml)))
                .unwrap());
        }
    };

    // Display messages from LLM
    for msg in execution_result.messages {
        let _ = status_tx.send(msg);
    }

    // Take the first XML document any action produced. ActionResult::Multiple is
    // unwrapped: a nested Output used to fall through to "no response generated".
    fn first_output(result: &ActionResult) -> Option<String> {
        match result {
            ActionResult::Output(bytes) => Some(String::from_utf8_lossy(bytes).to_string()),
            ActionResult::Multiple(inner) => inner.iter().find_map(first_output),
            _ => None,
        }
    }
    let mut response_xml = execution_result
        .protocol_results
        .iter()
        .find_map(first_output)
        .unwrap_or_default();

    // If no XML response was generated, return a fault
    if response_xml.is_empty() {
        tracing::warn!(
            "XML-RPC: model produced no response action for method {} (decision=fail_closed_no_action, fault_code={})",
            method_call.method_name,
            FAULT_INTERNAL_ERROR
        );
        Log::new(Some(&status_tx)).warn(
            "XML-RPC: LLM did not generate a response (decision=fail_closed_no_action)".to_string(),
        );
        response_xml = generate_fault(
            FAULT_INTERNAL_ERROR,
            crate::utils::WireFailure::Unavailable.text(),
        );
    }

    let log = Log::new(Some(&status_tx));
    log.trace(format!("XML-RPC response:\n{}", response_xml));
    log.debug(format!(
        "XML-RPC {} -> {} bytes",
        method_call.method_name,
        response_xml.len()
    ));

    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/xml; charset=utf-8")
        .body(Full::new(Bytes::from(response_xml)))
        .unwrap())
}

/// XML-RPC method call structure
#[derive(Debug, Clone)]
pub struct MethodCall {
    pub method_name: String,
    pub params: Vec<XmlRpcValue>,
}

/// XML-RPC value types
#[derive(Debug, Clone, PartialEq)]
pub enum XmlRpcValue {
    Int(i32),
    I8(i64), // Extension: 64-bit integer
    Boolean(bool),
    String(String),
    Double(f64),
    DateTime(String), // ISO 8601 format
    Base64(Vec<u8>),
    Array(Vec<XmlRpcValue>),
    Struct(Vec<(String, XmlRpcValue)>), // key-value pairs
    Nil,                                // Extension: null value
}

/// Maximum `<array>`/`<struct>`/`<value>` nesting accepted from a client.
///
/// The parser is iterative, so deep nesting cannot overflow the stack, but each
/// level costs a heap frame; this bounds what one request can allocate.
#[cfg(feature = "xmlrpc")]
const MAX_VALUE_DEPTH: usize = 64;

/// A container currently being filled.
#[cfg(feature = "xmlrpc")]
enum Container {
    Array(Vec<XmlRpcValue>),
    Struct {
        members: Vec<(String, XmlRpcValue)>,
        pending_name: Option<String>,
    },
}

/// Parse an XML-RPC `<methodCall>`.
///
/// The previous implementation pushed `XmlRpcValue::String(text)` for every text
/// node and never looked at the type element, so `<int>5</int>` reached the model
/// as `"5"` and six of the ten `XmlRpcValue` variants were unreachable. It also
/// collected array elements only at `</data>` (keeping just the last one and
/// leaking the rest into the next parameter), dropped parameters whose value was
/// empty (shifting every later parameter down one position), and returned
/// `Ok(MethodCall { method_name: "", .. })` for input that was not XML-RPC at all.
///
/// This version tracks the active type element, closes each value at `</value>`,
/// and validates the document shape.
#[cfg(feature = "xmlrpc")]
fn parse_method_call(xml: &str) -> Result<MethodCall> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut method_name: Option<String> = None;
    let mut params: Vec<XmlRpcValue> = Vec::new();
    let mut buf = Vec::new();

    let mut saw_method_call = false;
    let mut method_call_closed = false;
    let mut in_method_name = false;
    let mut in_member_name = false;

    // One frame per open <value>; the frame holds the value once its type element
    // has been closed.
    let mut value_frames: Vec<Option<XmlRpcValue>> = Vec::new();
    let mut containers: Vec<Container> = Vec::new();
    // The type element currently open inside the innermost <value>, if any.
    let mut current_type: Option<Vec<u8>> = None;
    let mut current_param: Option<XmlRpcValue> = None;

    /// Attach a completed value to the innermost open `<value>`.
    fn set_frame(frames: &mut [Option<XmlRpcValue>], value: XmlRpcValue) {
        if let Some(frame) = frames.last_mut() {
            *frame = Some(value);
        }
    }

    /// A closed `<value>` belongs to the enclosing container, or to the parameter.
    fn emit(
        containers: &mut [Container],
        current_param: &mut Option<XmlRpcValue>,
        value: XmlRpcValue,
    ) {
        match containers.last_mut() {
            Some(Container::Array(items)) => items.push(value),
            Some(Container::Struct {
                members,
                pending_name,
            }) => {
                let name = pending_name.take().unwrap_or_default();
                members.push((name, value));
            }
            None => *current_param = Some(value),
        }
    }

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(XmlEvent::Start(ref e)) => {
                let name = e.name().as_ref().to_vec();
                match name.as_slice() {
                    b"methodCall" => saw_method_call = true,
                    b"methodName" => in_method_name = true,
                    b"param" => current_param = None,
                    // The depth guard is on every nesting element, not only `<value>`.
                    // `<array>` and `<struct>` used to push a container with no check at all,
                    // and a well-formed document may open one without an enclosing `<value>`:
                    // `<array><array><array>…` is 7 bytes a level, so a body at the 4 MiB cap
                    // pushed ~600 000 frames. Bounded by the body cap rather than unbounded,
                    // but it is allocation a peer chooses and nothing needed.
                    b"value" | b"array" | b"struct" => {
                        if value_frames.len() + containers.len() >= MAX_VALUE_DEPTH {
                            return Err(anyhow::anyhow!(
                                "value nesting deeper than {} levels",
                                MAX_VALUE_DEPTH
                            ));
                        }
                        match name.as_slice() {
                            b"value" => {
                                value_frames.push(None);
                                current_type = None;
                            }
                            b"array" => containers.push(Container::Array(Vec::new())),
                            _ => containers.push(Container::Struct {
                                members: Vec::new(),
                                pending_name: None,
                            }),
                        }
                    }
                    b"name" => in_member_name = true,
                    b"i4" | b"int" | b"i8" | b"boolean" | b"string" | b"double"
                    | b"dateTime.iso8601" | b"base64" | b"nil" => {
                        current_type = Some(name);
                    }
                    _ => {}
                }
            }
            Ok(XmlEvent::End(ref e)) => match e.name().as_ref() {
                b"methodCall" => method_call_closed = true,
                b"methodName" => in_method_name = false,
                b"name" => in_member_name = false,
                b"value" => {
                    // An empty <value/> or <value></value> is the empty string
                    // (spec: "If no type is indicated, the type is string").
                    let value = value_frames
                        .pop()
                        .flatten()
                        .unwrap_or_else(|| XmlRpcValue::String(String::new()));
                    emit(&mut containers, &mut current_param, value);
                    current_type = None;
                }
                b"param" => {
                    params.push(
                        current_param
                            .take()
                            .unwrap_or_else(|| XmlRpcValue::String(String::new())),
                    );
                }
                b"array" => {
                    if let Some(Container::Array(items)) = containers.pop() {
                        set_frame(&mut value_frames, XmlRpcValue::Array(items));
                    }
                }
                b"struct" => {
                    if let Some(Container::Struct { members, .. }) = containers.pop() {
                        set_frame(&mut value_frames, XmlRpcValue::Struct(members));
                    }
                }
                tag @ (b"i4" | b"int" | b"i8" | b"boolean" | b"string" | b"double"
                | b"dateTime.iso8601" | b"base64" | b"nil") => {
                    // No text node arrived, e.g. <string></string> or <int></int>.
                    if value_frames.last().map(|f| f.is_none()).unwrap_or(false) {
                        set_frame(&mut value_frames, empty_typed_value(tag));
                    }
                    current_type = None;
                }
                _ => {}
            },
            Ok(XmlEvent::Text(e)) => {
                let text = e.unescape()?.to_string();
                if in_method_name {
                    method_name = Some(text);
                } else if in_member_name {
                    if let Some(Container::Struct { pending_name, .. }) = containers.last_mut() {
                        *pending_name = Some(text);
                    }
                } else if !value_frames.is_empty() {
                    let value = typed_value(current_type.as_deref(), &text)?;
                    set_frame(&mut value_frames, value);
                }
            }
            Ok(XmlEvent::Empty(ref e)) => {
                // Self-closing type element: <nil/>, <string/>, <int/>.
                let tag = e.name().as_ref().to_vec();
                if !value_frames.is_empty() {
                    match tag.as_slice() {
                        b"i4" | b"int" | b"i8" | b"boolean" | b"string" | b"double"
                        | b"dateTime.iso8601" | b"base64" | b"nil" => {
                            set_frame(&mut value_frames, empty_typed_value(&tag));
                        }
                        b"value" => {
                            emit(
                                &mut containers,
                                &mut current_param,
                                XmlRpcValue::String(String::new()),
                            );
                        }
                        _ => {}
                    }
                } else if tag.as_slice() == b"value" {
                    emit(
                        &mut containers,
                        &mut current_param,
                        XmlRpcValue::String(String::new()),
                    );
                }
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {}", e)),
            _ => {}
        }
        buf.clear();
    }

    // Reject anything that is not a well-formed methodCall rather than reporting a
    // call to the empty method name. A browser GET, a JSON body or a truncated
    // document all used to arrive at the model as method "" with no parameters.
    if !saw_method_call {
        return Err(anyhow::anyhow!("not an XML-RPC methodCall document"));
    }
    // quick-xml reports EOF rather than an error when elements are still open, so
    // a body truncated mid-document used to be handled as a complete call.
    if !method_call_closed {
        return Err(anyhow::anyhow!("truncated methodCall document"));
    }
    let method_name = match method_name {
        Some(name) if !name.is_empty() => name,
        _ => return Err(anyhow::anyhow!("methodCall has no methodName")),
    };
    if !value_frames.is_empty() || !containers.is_empty() {
        return Err(anyhow::anyhow!("unterminated value, array or struct"));
    }

    Ok(MethodCall {
        method_name,
        params,
    })
}

/// Decode a text node according to the type element that encloses it.
#[cfg(feature = "xmlrpc")]
fn typed_value(tag: Option<&[u8]>, text: &str) -> Result<XmlRpcValue> {
    let parse_err = |ty: &str| anyhow::anyhow!("invalid <{}> value: {:?}", ty, text);
    Ok(match tag {
        // Untyped text inside <value> is a string, per the specification.
        None | Some(b"string") => XmlRpcValue::String(text.to_string()),
        Some(b"i4") | Some(b"int") => {
            XmlRpcValue::Int(text.trim().parse::<i32>().map_err(|_| parse_err("int"))?)
        }
        Some(b"i8") => XmlRpcValue::I8(text.trim().parse::<i64>().map_err(|_| parse_err("i8"))?),
        Some(b"boolean") => match text.trim() {
            "1" | "true" => XmlRpcValue::Boolean(true),
            "0" | "false" => XmlRpcValue::Boolean(false),
            _ => return Err(parse_err("boolean")),
        },
        Some(b"double") => {
            let d = text
                .trim()
                .parse::<f64>()
                .map_err(|_| parse_err("double"))?;
            if !d.is_finite() {
                return Err(parse_err("double"));
            }
            XmlRpcValue::Double(d)
        }
        Some(b"dateTime.iso8601") => XmlRpcValue::DateTime(text.trim().to_string()),
        Some(b"base64") => {
            use base64::Engine;
            XmlRpcValue::Base64(
                base64::engine::general_purpose::STANDARD
                    .decode(text.trim())
                    .map_err(|_| parse_err("base64"))?,
            )
        }
        Some(b"nil") => XmlRpcValue::Nil,
        Some(_) => XmlRpcValue::String(text.to_string()),
    })
}

/// The value of a type element that contains no text, e.g. `<string/>`.
#[cfg(feature = "xmlrpc")]
fn empty_typed_value(tag: &[u8]) -> XmlRpcValue {
    match tag {
        b"i4" | b"int" => XmlRpcValue::Int(0),
        b"i8" => XmlRpcValue::I8(0),
        b"boolean" => XmlRpcValue::Boolean(false),
        b"double" => XmlRpcValue::Double(0.0),
        b"base64" => XmlRpcValue::Base64(Vec::new()),
        b"nil" => XmlRpcValue::Nil,
        _ => XmlRpcValue::String(String::new()),
    }
}

/// Generate an XML-RPC fault response.
///
/// The message is XML-escaped. It used to be interpolated raw with `format!`, so a
/// fault_string from the model containing `<`, `>` or `&` — "Unknown method: <foo>"
/// is entirely plausible — produced a document no client could parse. The same
/// applied to the parse-error and internal-error paths, which embed the text of a
/// `serde`/`quick-xml` error.
#[cfg(feature = "xmlrpc")]
pub fn generate_fault(code: i32, message: &str) -> String {
    format!(
        r#"<?xml version="1.0"?>
<methodResponse>
  <fault>
    <value>
      <struct>
        <member>
          <name>faultCode</name>
          <value><int>{}</int></value>
        </member>
        <member>
          <name>faultString</name>
          <value><string>{}</string></value>
        </member>
      </struct>
    </value>
  </fault>
</methodResponse>"#,
        code,
        escape_xml_text(message)
    )
}

/// Escape the five XML predefined entities in character data.
#[cfg(feature = "xmlrpc")]
fn escape_xml_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Generate XML-RPC success response with a value
#[cfg(feature = "xmlrpc")]
pub fn generate_success_response(value: &XmlRpcValue) -> String {
    let mut writer = Writer::new(Cursor::new(Vec::new()));

    // XML declaration
    writer
        .write_event(XmlEvent::Decl(quick_xml::events::BytesDecl::new(
            "1.0", None, None,
        )))
        .unwrap();

    // methodResponse
    writer
        .write_event(XmlEvent::Start(BytesStart::new("methodResponse")))
        .unwrap();
    writer
        .write_event(XmlEvent::Start(BytesStart::new("params")))
        .unwrap();
    writer
        .write_event(XmlEvent::Start(BytesStart::new("param")))
        .unwrap();

    write_value(&mut writer, value);

    writer
        .write_event(XmlEvent::End(BytesEnd::new("param")))
        .unwrap();
    writer
        .write_event(XmlEvent::End(BytesEnd::new("params")))
        .unwrap();
    writer
        .write_event(XmlEvent::End(BytesEnd::new("methodResponse")))
        .unwrap();

    String::from_utf8(writer.into_inner().into_inner()).unwrap()
}

/// Write XML-RPC value to XML writer
#[cfg(feature = "xmlrpc")]
fn write_value(writer: &mut Writer<Cursor<Vec<u8>>>, value: &XmlRpcValue) {
    writer
        .write_event(XmlEvent::Start(BytesStart::new("value")))
        .unwrap();

    match value {
        XmlRpcValue::Int(i) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("int")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(&i.to_string())))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("int")))
                .unwrap();
        }
        XmlRpcValue::I8(i) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("i8")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(&i.to_string())))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("i8")))
                .unwrap();
        }
        XmlRpcValue::Boolean(b) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("boolean")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(if *b { "1" } else { "0" })))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("boolean")))
                .unwrap();
        }
        XmlRpcValue::String(s) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("string")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(s)))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("string")))
                .unwrap();
        }
        XmlRpcValue::Double(d) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("double")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(&d.to_string())))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("double")))
                .unwrap();
        }
        XmlRpcValue::DateTime(dt) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("dateTime.iso8601")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(dt)))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("dateTime.iso8601")))
                .unwrap();
        }
        XmlRpcValue::Base64(bytes) => {
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            writer
                .write_event(XmlEvent::Start(BytesStart::new("base64")))
                .unwrap();
            writer
                .write_event(XmlEvent::Text(BytesText::new(&encoded)))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("base64")))
                .unwrap();
        }
        XmlRpcValue::Array(arr) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("array")))
                .unwrap();
            writer
                .write_event(XmlEvent::Start(BytesStart::new("data")))
                .unwrap();
            for item in arr {
                write_value(writer, item);
            }
            writer
                .write_event(XmlEvent::End(BytesEnd::new("data")))
                .unwrap();
            writer
                .write_event(XmlEvent::End(BytesEnd::new("array")))
                .unwrap();
        }
        XmlRpcValue::Struct(members) => {
            writer
                .write_event(XmlEvent::Start(BytesStart::new("struct")))
                .unwrap();
            for (name, val) in members {
                writer
                    .write_event(XmlEvent::Start(BytesStart::new("member")))
                    .unwrap();
                writer
                    .write_event(XmlEvent::Start(BytesStart::new("name")))
                    .unwrap();
                writer
                    .write_event(XmlEvent::Text(BytesText::new(name)))
                    .unwrap();
                writer
                    .write_event(XmlEvent::End(BytesEnd::new("name")))
                    .unwrap();
                write_value(writer, val);
                writer
                    .write_event(XmlEvent::End(BytesEnd::new("member")))
                    .unwrap();
            }
            writer
                .write_event(XmlEvent::End(BytesEnd::new("struct")))
                .unwrap();
        }
        XmlRpcValue::Nil => {
            writer
                .write_event(XmlEvent::Empty(BytesStart::new("nil")))
                .unwrap();
        }
    }

    writer
        .write_event(XmlEvent::End(BytesEnd::new("value")))
        .unwrap();
}
