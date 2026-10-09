//! FastCGI 1.0 client in the web-server role.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::fastcgi::record;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::FastcgiClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_DOCUMENT_ROOT: &str = "/var/www/html";
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an aborted request may take to send its END_REQUEST.
const ABORT_GRACE: Duration = Duration::from_secs(5);

struct Settings {
    remote: String,
    document_root: String,
    timeout: Duration,
}

async fn dial(remote: &str) -> Result<TcpStream> {
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(remote))
        .await
        .context("FastCGI connect timed out")?
        .with_context(|| format!("FastCGI connect to {remote}"))
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let document_root = p
        .map(|p| p.get_optional_string("document_root"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_DOCUMENT_ROOT.to_owned());
    anyhow::ensure!(
        document_root.starts_with('/') && document_root.len() <= 1024,
        "document_root must be an absolute path"
    );
    let timeout = p
        .map(|p| p.get_optional_u64("request_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=600).contains(&timeout),
        "request_timeout_secs must be 1..=600"
    );
    let stream = dial(&ctx.remote_addr).await?;
    let local = stream.local_addr()?;
    let settings = Settings {
        remote: ctx.remote_addr.clone(),
        document_root: document_root.trim_end_matches('/').to_owned(),
        timeout: Duration::from_secs(timeout),
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr": ctx.remote_addr}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = FastcgiClientProtocol;
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
                &protocol,
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
                    .warn(format!("FastCGI client handler: {e}")),
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
            &settings,
            Some(stream),
            external,
            internal_rx,
            event_tx,
        )
        .await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("FastCGI client ended: {e}"));
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
    settings: &Settings,
    mut stream: Option<TcpStream>,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut next_id: u16 = 1;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        match FastcgiClientProtocol.execute_action(action.clone()) {
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
        let is_request = action["type"] == "fastcgi_request";
        let request_id = next_id;
        if is_request {
            next_id = next_id.checked_add(1).unwrap_or(1);
        }
        // An application may close an idle kept connection; one reconnect per action.
        let mut outcome = None;
        for attempt in 0..2 {
            if stream.is_none() {
                match dial(&settings.remote).await {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        outcome = Some(Err(e));
                        break;
                    }
                }
            }
            let s = stream.as_mut().expect("dialled above");
            let r = if is_request {
                request(s, settings, request_id, &action).await
            } else {
                get_values(s, &action).await
            };
            match r {
                Err(Failure::Transport(e)) if attempt == 0 => {
                    Log::new(Some(&ctx.status_tx)).debug(format!("FastCGI reconnecting: {e}"));
                    stream = None;
                }
                Err(Failure::Transport(e)) => {
                    stream = None;
                    outcome = Some(Err(e));
                    break;
                }
                Ok((data, keep)) => {
                    if !keep {
                        stream = None;
                    }
                    outcome = Some(Ok(data));
                    break;
                }
            }
        }
        let outcome = outcome.unwrap_or_else(|| Err(anyhow::anyhow!("no attempt made")));
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "FastCGI",
                    None,
                    "injected_action",
                    json!({"type": action["type"], "path": action["path"]}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                reply(command, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                let kind = if is_request {
                    &*actions::RESPONSE_EVENT
                } else {
                    &*actions::VALUES_EVENT
                };
                events
                    .send(Event::new(kind, data))
                    .await
                    .context("FastCGI event consumer stopped")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("FastCGI request failed: {e}"));
                reply(command, Err(e));
            }
        }
    }
}

enum Failure {
    /// The connection failed before any answer: worth one reconnect.
    Transport(anyhow::Error),
}

fn params_for(
    settings: &Settings,
    action: &Value,
    body_len: usize,
) -> Result<Vec<(String, String)>> {
    let method = action["method"].as_str().unwrap_or("GET");
    let path = action["path"].as_str().unwrap_or("/");
    let query = action["query"].as_str().unwrap_or("");
    let (host, port) = settings
        .remote
        .rsplit_once(':')
        .unwrap_or((settings.remote.as_str(), ""));
    let mut params: Vec<(String, String)> = vec![
        ("GATEWAY_INTERFACE".into(), "CGI/1.1".into()),
        ("SERVER_SOFTWARE".into(), "netget".into()),
        ("SERVER_PROTOCOL".into(), "HTTP/1.1".into()),
        ("REQUEST_METHOD".into(), method.into()),
        (
            "REQUEST_URI".into(),
            if query.is_empty() {
                path.into()
            } else {
                format!("{path}?{query}")
            },
        ),
        ("SCRIPT_NAME".into(), path.into()),
        (
            "SCRIPT_FILENAME".into(),
            format!("{}{path}", settings.document_root),
        ),
        ("DOCUMENT_ROOT".into(), settings.document_root.clone()),
        ("PATH_INFO".into(), String::new()),
        ("QUERY_STRING".into(), query.into()),
        ("SERVER_NAME".into(), host.trim_matches(['[', ']']).into()),
        ("SERVER_PORT".into(), port.into()),
        ("REMOTE_ADDR".into(), "127.0.0.1".into()),
        (
            "CONTENT_LENGTH".into(),
            if body_len > 0 {
                body_len.to_string()
            } else {
                String::new()
            },
        ),
    ];
    for (k, v) in record::check_headers(action.get("headers"))? {
        if k.eq_ignore_ascii_case("content-type") {
            params.push(("CONTENT_TYPE".into(), v));
        } else if !k.eq_ignore_ascii_case("content-length") {
            params.push((
                format!("HTTP_{}", k.to_ascii_uppercase().replace('-', "_")),
                v,
            ));
        }
    }
    for (k, v) in record::check_headers(action.get("params"))? {
        params.retain(|(name, _)| *name != k);
        params.push((k, v));
    }
    Ok(params)
}

async fn send(s: &mut TcpStream, bytes: &[u8]) -> Result<(), Failure> {
    s.write_all(bytes)
        .await
        .map_err(|e| Failure::Transport(e.into()))?;
    s.flush().await.map_err(|e| Failure::Transport(e.into()))
}

/// One Responder request. `Ok` carries the event data and whether the connection stays usable.
async fn request(
    s: &mut TcpStream,
    settings: &Settings,
    id: u16,
    action: &Value,
) -> Result<(Value, bool), Failure> {
    let failed = |e: anyhow::Error| Ok((json!({"request_id": id, "error": e.to_string()}), false));
    let body = match record::decode_body(action) {
        Ok(b) => b,
        Err(e) => return failed(e),
    };
    let params = match params_for(settings, action, body.len()) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    let pairs = record::encode_pairs(params.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if pairs.len() > record::MAX_PARAMS_BYTES {
        return failed(anyhow::anyhow!("params exceed 64 KiB"));
    }
    let mut out = record::encode(
        record::BEGIN_REQUEST,
        id,
        &record::begin_request(record::ROLE_RESPONDER, record::KEEP_CONN),
    );
    out.extend(record::encode_stream(record::PARAMS, id, &pairs));
    out.extend(record::encode_stream(record::STDIN, id, &body));
    send(s, &out).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut aborted = false;
    let mut deadline = tokio::time::Instant::now() + settings.timeout;
    let mut first = true;
    let (app_status, protocol_status) = loop {
        let rec = match tokio::time::timeout_at(deadline, record::read_record(s)).await {
            Err(_) if !aborted => {
                aborted = true;
                send(s, &record::encode(record::ABORT_REQUEST, id, &[])).await?;
                deadline = tokio::time::Instant::now() + ABORT_GRACE;
                continue;
            }
            Err(_) => {
                return Ok((
                    json!({"request_id": id, "aborted": true, "error": "no END_REQUEST after ABORT_REQUEST"}),
                    false,
                ));
            }
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) if first => {
                return Err(Failure::Transport(anyhow::anyhow!(
                    "application closed the connection"
                )))
            }
            Ok(Ok(None)) => {
                return failed(anyhow::anyhow!(
                    "application closed the connection mid-response"
                ))
            }
            Ok(Err(e)) if first => return Err(Failure::Transport(e)),
            Ok(Err(e)) => return failed(e),
        };
        first = false;
        if rec.request_id != id {
            continue;
        }
        match rec.kind {
            record::STDOUT => {
                if stdout.len() + rec.content.len() > record::MAX_BODY_BYTES + 64 * 1024 {
                    return failed(anyhow::anyhow!("response over 1 MiB"));
                }
                stdout.extend_from_slice(&rec.content);
            }
            record::STDERR => {
                let room = record::MAX_STDERR_BYTES.saturating_sub(stderr.len());
                stderr.extend_from_slice(&rec.content[..rec.content.len().min(room)]);
            }
            record::END_REQUEST => {
                if rec.content.len() < 8 {
                    return failed(anyhow::anyhow!("END_REQUEST body is 8 bytes"));
                }
                let c = &rec.content;
                break (u32::from_be_bytes([c[0], c[1], c[2], c[3]]), c[4]);
            }
            _ => {}
        }
    };
    let mut data = json!({
        "request_id": id,
        "app_status": app_status,
        "protocol_status": record::protocol_status_name(protocol_status),
        "stderr": String::from_utf8_lossy(&stderr),
    });
    if aborted {
        data["aborted"] = json!(true);
    }
    if protocol_status == record::REQUEST_COMPLETE && !stdout.is_empty() {
        match record::parse_cgi_response(&stdout) {
            Ok((status, headers, body)) => {
                let (text, encoding) = record::body_text(&body);
                data["status"] = json!(status);
                data["headers"] = Value::Object(headers);
                data["body"] = json!(text);
                data["body_encoding"] = json!(encoding);
            }
            Err(e) => data["error"] = json!(format!("not a CGI response: {e}")),
        }
    }
    Ok((data, true))
}

async fn get_values(s: &mut TcpStream, action: &Value) -> Result<(Value, bool), Failure> {
    let names: Vec<String> = match action["names"].as_array() {
        Some(list) => list
            .iter()
            .filter_map(|n| n.as_str().map(str::to_owned))
            .collect(),
        None => ["FCGI_MAX_CONNS", "FCGI_MAX_REQS", "FCGI_MPXS_CONNS"]
            .map(String::from)
            .to_vec(),
    };
    let body = record::encode_pairs(names.iter().map(|n| (n.as_str(), "")));
    send(s, &record::encode(record::GET_VALUES, 0, &body)).await?;
    let deadline = tokio::time::Instant::now() + ABORT_GRACE;
    loop {
        let rec = match tokio::time::timeout_at(deadline, record::read_record(s)).await {
            Err(_) => {
                return Ok((
                    json!({"values": {}, "error": "no FCGI_GET_VALUES_RESULT"}),
                    true,
                ))
            }
            Ok(Ok(Some(r))) => r,
            Ok(Ok(None)) => {
                return Err(Failure::Transport(anyhow::anyhow!(
                    "application closed the connection"
                )))
            }
            Ok(Err(e)) => return Err(Failure::Transport(e)),
        };
        if rec.request_id == 0 && rec.kind == record::GET_VALUES_RESULT {
            let values: Map<String, Value> = match record::decode_pairs(&rec.content) {
                Ok(pairs) => pairs.into_iter().map(|(k, v)| (k, json!(v))).collect(),
                Err(e) => return Ok((json!({"values": {}, "error": e.to_string()}), false)),
            };
            return Ok((json!({"values": values}), true));
        }
        if rec.request_id == 0 && rec.kind == record::UNKNOWN_TYPE {
            return Ok((
                json!({"values": {}, "error": "the application does not know FCGI_GET_VALUES"}),
                true,
            ));
        }
    }
}
