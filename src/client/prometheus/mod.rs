pub mod actions;
pub mod exposition;
use crate::client::{command_support, llm_budget::call_llm_for_client};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
pub use actions::PrometheusClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
pub const DEFAULT_PATH: &str = "/metrics";
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const MAX_TIMEOUT_SECS: u64 = 30;
pub const MAX_FOLLOWUPS: u8 = 4;
pub const QUEUE_CAPACITY: usize = 8;
pub const MAX_PATH: usize = 4096;
#[derive(Debug, Clone)]
pub struct Request {
    pub path: String,
    pub format: String,
}
pub fn request(action: &Value, default_path: &str) -> Result<Request> {
    ensure!(
        action["type"] == "scrape_metrics",
        "expected scrape_metrics action"
    );
    let path = match action.get("path") {
        None => default_path,
        Some(v) => v.as_str().context("path must be string")?,
    };
    ensure!(
        !path.is_empty()
            && path.len() <= MAX_PATH
            && path.starts_with('/')
            && !path.starts_with("//")
            && !path.contains('#'),
        "scrape path must be a bounded origin path/query"
    );
    let uri: hyper::Uri = path.parse().context("invalid scrape path")?;
    ensure!(
        uri.scheme().is_none() && uri.authority().is_none(),
        "scrape path must stay on exporter origin"
    );
    let format = match action.get("format") {
        None => "auto",
        Some(v) => v.as_str().context("format must be string")?,
    };
    ensure!(
        ["auto", "text", "openmetrics"].contains(&format),
        "format must be auto/text/openmetrics"
    );
    Ok(Request {
        path: path.into(),
        format: format.into(),
    })
}
fn accept(format: &str) -> &'static str {
    match format {
        "text"=>"text/plain;version=0.0.4;escaping=underscores",
        "openmetrics"=>"application/openmetrics-text;version=1.0.0;escaping=underscores",
        _=>"application/openmetrics-text;version=1.0.0;escaping=underscores;q=1.0,text/plain;version=0.0.4;escaping=underscores;q=0.8",
    }
}
fn parse_response(
    status: u16,
    headers: &hyper::HeaderMap,
    body: &[u8],
    requested: &str,
) -> Result<Value> {
    ensure!(status == 200, "exporter returned HTTP {status}");
    ensure!(
        headers.get_all("content-type").iter().count() == 1,
        "exporter must send exactly one Content-Type"
    );
    let content_type = headers
        .get("content-type")
        .context("missing Content-Type")?
        .to_str()?;
    let format = exposition::content_type(content_type)?;
    ensure!(
        requested == "auto" || requested == format.name(),
        "exporter returned an unrequested metrics format"
    );
    for encoding in headers.get_all("content-encoding") {
        ensure!(
            encoding.to_str()?.eq_ignore_ascii_case("identity"),
            "unsupported metrics Content-Encoding; requested identity"
        );
    }
    let mut data = exposition::parse(body, format)?;
    data["content_type"] = json!(content_type);
    Ok(data)
}
#[cfg(not(target_arch = "wasm32"))]
async fn fetch(
    client: &reqwest::Client,
    origin: &str,
    request: &Request,
    timeout: Duration,
) -> Result<Value> {
    let mut response = client
        .get(format!("{origin}{}", request.path))
        .header("Accept", accept(&request.format))
        .header("Accept-Encoding", "identity")
        .header(
            "X-Prometheus-Scrape-Timeout-Seconds",
            timeout.as_secs().to_string(),
        )
        .header("Connection", "close")
        .send()
        .await
        .context("send exporter scrape")?;
    let status = response.status().as_u16();
    ensure!(status == 200, "exporter returned HTTP {status}");
    if let Some(length) = response.content_length() {
        ensure!(length <= exposition::MAX_BODY as u64, "metrics body limit");
    }
    let headers = response.headers().clone();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read exporter body")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= exposition::MAX_BODY,
            "metrics body limit"
        );
        body.extend_from_slice(&chunk);
    }
    parse_response(status, &headers, &body, &request.format)
}
#[cfg(target_arch = "wasm32")]
async fn fetch(_client: &(), origin: &str, request: &Request, timeout: Duration) -> Result<Value> {
    use http_body_util::{BodyExt, Full, Limited};
    use hyper_util::rt::TokioIo;
    let target =
        crate::client::http_fetch::transport::parse_http_url(&format!("{origin}{}", request.path))?;
    let stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
    // Drive the socket as an owned future, with no detached task on timeout/cancellation.
    let exchange = async {
        let response = sender
            .send_request(
                hyper::Request::builder()
                    .method("GET")
                    .uri(target.path_and_query.as_str())
                    .header("Host", target.authority.as_str())
                    .header("Accept", accept(&request.format))
                    .header("Accept-Encoding", "identity")
                    .header(
                        "X-Prometheus-Scrape-Timeout-Seconds",
                        timeout.as_secs().to_string(),
                    )
                    .header("Connection", "close")
                    .body(Full::new(bytes::Bytes::new()))?,
            )
            .await?;
        let (parts, body) = response.into_parts();
        let body = Limited::new(body, exposition::MAX_BODY)
            .collect()
            .await?
            .to_bytes();
        parse_response(
            parts.status.as_u16(),
            &parts.headers,
            &body,
            &request.format,
        )
    };
    tokio::pin!(exchange, connection);
    tokio::select! {result=&mut exchange=>result,result=&mut connection=>{result?;anyhow::bail!("exporter connection closed before complete scrape")}}
}
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let address = if ctx.remote_addr.contains("://") {
        ctx.remote_addr.clone()
    } else {
        format!("http://{}", ctx.remote_addr)
    };
    let url = url::Url::parse(&address).context("invalid exporter URL")?;
    ensure!(
        ["http", "https"].contains(&url.scheme()) && url.host_str().is_some(),
        "exporter URL must be HTTP(S)"
    );
    ensure!(url.username().is_empty() && url.password().is_none() && url.fragment().is_none() && url.query().is_none() && url.path()=="/","use an exporter origin without credentials/path/query; metrics_path selects the scrape endpoint");
    let origin = url.origin().ascii_serialization();
    let mut default_path = DEFAULT_PATH.to_owned();
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    if let Some(p) = &ctx.startup_params {
        default_path = p
            .get_optional_string("metrics_path")?
            .unwrap_or_else(|| DEFAULT_PATH.into());
        timeout_secs = p
            .get_optional_u64("scrape_timeout_secs")?
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
    }
    request(
        &json!({"type":"scrape_metrics","path":default_path}),
        DEFAULT_PATH,
    )?;
    ensure!(
        (1..=MAX_TIMEOUT_SECS).contains(&timeout_secs),
        "scrape_timeout_secs must be 1..30"
    );
    let timeout = Duration::from_secs(timeout_secs);
    #[cfg(target_arch = "wasm32")]
    crate::client::http_fetch::transport::parse_http_url(&origin)?;
    let host = url
        .host_str()
        .unwrap()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let remote = tokio::time::timeout(
        timeout,
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap())),
    )
    .await
    .context("exporter resolution deadline")??
    .next()
    .context("exporter resolved no addresses")?;
    #[cfg(not(target_arch = "wasm32"))]
    let client = reqwest::Client::builder()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .pool_max_idle_per_host(0)
        .http1_only()
        .user_agent("NetGet-Prometheus/1.0")
        .build()?;
    #[cfg(target_arch = "wasm32")]
    let client = ();
    let external = command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (actions_tx, actions_rx) = mpsc::channel::<HandlerAction>(QUEUE_CAPACITY);
    let (events_tx, mut events_rx) = mpsc::channel::<HandlerEvent>(QUEUE_CAPACITY);
    let handler_ctx = ctx.clone();
    let handler = tokio::spawn(async move {
        while let Some((event, depth)) = events_rx.recv().await {
            let instruction = handler_ctx
                .state
                .get_instruction_for_client(handler_ctx.client_id)
                .await
                .unwrap_or_default();
            let memory = handler_ctx
                .state
                .get_memory_for_client(handler_ctx.client_id)
                .await
                .unwrap_or_default();
            match call_llm_for_client(
                &handler_ctx.llm_client,
                &handler_ctx.state,
                handler_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                &PrometheusClientProtocol,
                &handler_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(memory) = result.memory_updates {
                        handler_ctx
                            .state
                            .set_memory_for_client(handler_ctx.client_id, memory)
                            .await;
                    }
                    for action in result.actions {
                        if depth >= MAX_FOLLOWUPS && action["type"] != "disconnect" {
                            Log::new(Some(&handler_ctx.status_tx))
                                .warn("Prometheus handler followup limit reached");
                            continue;
                        }
                        if actions_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&handler_ctx.status_tx)).warn(format!("Prometheus handler: {e}"))
                }
            }
        }
    });
    let handler_abort = handler.abort_handle();
    ctx.state.register_client_task(ctx.client_id, handler).await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            &client,
            &origin,
            &default_path,
            timeout,
            external,
            actions_rx,
            events_tx,
        )
        .await;
        handler_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Prometheus ended: {e}"));
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
    Ok(remote)
}
#[cfg(not(target_arch = "wasm32"))]
type HttpClient = reqwest::Client;
#[cfg(target_arch = "wasm32")]
type HttpClient = ();
#[allow(clippy::too_many_arguments)]
async fn session(
    ctx: &ConnectContext,
    client: &HttpClient,
    origin: &str,
    default_path: &str,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    events
        .try_send((
            Event::new(
                &actions::CONNECTED_EVENT,
                json!({"origin":origin,"metrics_path":default_path}),
            ),
            0,
        ))
        .context("Prometheus event queue full")?;
    loop {
        let (action, depth, command) = tokio::select! {
            command=external.recv()=>match command {Some(c)=>(c.action.clone(),0,Some(c)),None=>return Ok(())},
            action=internal.recv()=>match action {Some((a,d))=>(a,d,None),None=>return Ok(())},
        };
        if action["type"] == "disconnect" {
            if let Some(c) = command {
                command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
            }
            return Ok(());
        }
        let request = match request(&action, default_path) {
            Ok(r) => r,
            Err(e) => {
                if let Some(c) = command {
                    command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                } else {
                    Log::new(Some(&ctx.status_tx)).warn(format!("Prometheus action rejected: {e}"));
                }
                continue;
            }
        };
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Prometheus",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![json!({"accepted":true})],
                )
                .await;
            command_support::reply(
                c,
                Ok(ClientSendOutcome::Executed {
                    detail: "scrape accepted; result follows as an event".into(),
                }),
            );
        }
        let result = {
            let scrape = tokio::time::timeout(timeout, fetch(client, origin, &request, timeout));
            tokio::pin!(scrape);
            loop {
                tokio::select! {biased;
                    command=external.recv()=>{
                        let Some(c)=command else{return Ok(())};
                        if c.action["type"]=="disconnect" {command_support::reply(c,Ok(ClientSendOutcome::Disconnected));return Ok(());}
                        command_support::reply(c,Ok(ClientSendOutcome::Rejected{error:"Prometheus scrape pending; retry after its event".into()}));
                    },
                    result=&mut scrape=>break result.context("whole exporter scrape deadline").and_then(|r|r),
                }
            }
        };
        let event = match result {
            Ok(mut data) => {
                data["request"] = action;
                data["path"] = json!(request.path);
                Event::new(&actions::METRICS_EVENT, data)
            }
            Err(e) => Event::new(
                &actions::ERROR_EVENT,
                json!({"request":action,"path":request.path,"error":format!("{e:#}")}),
            ),
        };
        events
            .try_send((event, depth))
            .context("Prometheus event queue full; disconnecting bounded client")?;
    }
}
