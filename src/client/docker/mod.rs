pub mod actions;
pub mod schema;
use crate::client::{command_support, llm_budget::call_llm_for_client};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::{
    client_handles::{ClientCommand, ClientSendOutcome},
    AccessLogOwner, ClientStatus,
};
pub use actions::DockerClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
pub const DEFAULT_API_VERSION: &str = "1.47";
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const MAX_TIMEOUT_SECS: u64 = 30;
pub const MAX_FOLLOWUPS: u8 = 4;
pub const QUEUE_CAPACITY: usize = 8;
pub fn api_version(v: &str) -> Result<(u16, u16)> {
    let (major, minor) = v
        .split_once('.')
        .context("API version must be major.minor")?;
    ensure!(
        !major.is_empty()
            && !minor.is_empty()
            && major.len() <= 3
            && minor.len() <= 3
            && major
                .bytes()
                .chain(minor.bytes())
                .all(|c| c.is_ascii_digit()),
        "invalid API version"
    );
    Ok((major.parse()?, minor.parse()?))
}
pub struct Request {
    pub operation: String,
    pub path: String,
}
pub fn request(action: &Value, version: &str) -> Result<Request> {
    api_version(version)?;
    ensure!(
        action["type"] == "docker_request",
        "expected docker_request action"
    );
    let operation = action["operation"]
        .as_str()
        .context("operation must be string")?;
    let (route, allowed) = match operation {
        "ping" => ("/_ping".to_owned(), vec![]),
        "version" => ("/version".into(), vec![]),
        "info" => ("/info".into(), vec![]),
        "containers" => (
            "/containers/json".into(),
            vec!["all", "limit", "size", "filters"],
        ),
        "container" => {
            let id = action["container_id"]
                .as_str()
                .context("container_id required")?;
            ensure!(
                id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
                    && id.len() <= 256
                    && id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)),
                "container_id must be a bounded ID or name"
            );
            (
                format!("/containers/{id}/json"),
                vec!["container_id", "size"],
            )
        }
        "images" => ("/images/json".into(), vec!["all", "digests", "filters"]),
        "networks" => ("/networks".into(), vec!["filters"]),
        "volumes" => ("/volumes".into(), vec!["filters"]),
        _ => anyhow::bail!("unknown read-only Docker operation"),
    };
    let object = action.as_object().context("action must be object")?;
    for field in object.keys() {
        ensure!(
            ["type", "operation"].contains(&field.as_str()) || allowed.contains(&field.as_str()),
            "field {field} is not allowed for {operation}"
        );
    }
    let mut query = Vec::new();
    for field in ["all", "size", "digests"] {
        if let Some(v) = action.get(field) {
            let value = v
                .as_bool()
                .with_context(|| format!("{field} must be boolean"))?;
            query.push(format!("{field}={value}"));
        }
    }
    if let Some(v) = action.get("limit") {
        let limit = v.as_u64().context("limit must be integer")?;
        ensure!((1..=1000).contains(&limit), "limit must be 1..1000");
        query.push(format!("limit={limit}"));
    }
    if let Some(v) = action.get("filters") {
        let filters = v
            .as_object()
            .context("filters must be object of string arrays")?;
        ensure!(filters.len() <= 64, "filter count limit");
        for (key, values) in filters {
            ensure!(
                !key.is_empty()
                    && key.len() <= 64
                    && key
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
                "invalid filter name"
            );
            let values = values
                .as_array()
                .context("filter values must be string arrays")?;
            ensure!(values.len() <= 64, "filter value count limit");
            for v in values {
                let text = v.as_str().context("filter value must be string")?;
                ensure!(
                    text.len() <= 1024 && !crate::utils::sanitize::has_controls(&text),
                    "filter value limit/control character"
                );
            }
        }
        let encoded = serde_json::to_string(v)?;
        ensure!(encoded.len() <= 8192, "encoded filter limit");
        query.push(format!("filters={}", urlencoding::encode(&encoded)));
    }
    let mut path = if operation == "ping" {
        route
    } else {
        format!("/v{version}{route}")
    };
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    Ok(Request {
        operation: operation.into(),
        path,
    })
}
#[derive(Clone)]
enum Endpoint {
    Tcp {
        origin: String,
        host: String,
        port: u16,
        authority: String,
    },
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    Unix(String),
}
impl Endpoint {
    fn parse(address: &str) -> Result<Self> {
        if let Some(path) = address.strip_prefix("unix://") {
            ensure!(
                path.starts_with('/')
                    && path.len() <= 4096
                    && !crate::utils::sanitize::has_controls(&path)
                    && !path.contains(['?', '#']),
                "Unix endpoint must be an absolute socket path"
            );
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            return Ok(Self::Unix(path.into()));
            #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
            anyhow::bail!("Unix Docker transport is unavailable on this platform");
        }
        let address = if address.contains("://") {
            address.into()
        } else {
            format!("http://{address}")
        };
        let url = url::Url::parse(&address).context("invalid Docker endpoint")?;
        ensure!(
            url.scheme() == "http" && url.host_str().is_some(),
            "Docker transport requires HTTP TCP or native unix://; HTTPS/TLS is not implemented"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none(),
            "Docker endpoint must be an origin without credentials/path/query"
        );
        let host = url.host_str().unwrap().trim_matches(['[', ']']).to_owned();
        let port = url.port_or_known_default().unwrap();
        let authority = url[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        Ok(Self::Tcp {
            origin: url.origin().ascii_serialization(),
            host,
            port,
            authority,
        })
    }
    fn name(&self) -> String {
        match self {
            Self::Tcp { origin, .. } => origin.clone(),
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            Self::Unix(path) => format!("unix://{path}"),
        }
    }
}
trait Socket: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> Socket for T {}
async fn fetch(
    endpoint: &Endpoint,
    method: &str,
    path: &str,
) -> Result<(u16, hyper::HeaderMap, Vec<u8>)> {
    use http_body_util::{BodyExt, Full, Limited};
    let (stream, authority): (Box<dyn Socket>, &str) = match endpoint {
        Endpoint::Tcp {
            host,
            port,
            authority,
            ..
        } => (
            Box::new(
                tokio::net::TcpStream::connect((host.as_str(), *port))
                    .await
                    .context("connect Docker TCP")?,
            ),
            authority,
        ),
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        Endpoint::Unix(path) => (
            Box::new(
                tokio::net::UnixStream::connect(path)
                    .await
                    .context("connect Docker Unix socket")?,
            ),
            "localhost",
        ),
    };
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    let exchange = async {
        let response = sender
            .send_request(
                hyper::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("Host", authority)
                    .header("Accept", "application/json")
                    .header("Accept-Encoding", "identity")
                    .header("User-Agent", "NetGet-Docker/1.0")
                    .header("Connection", "close")
                    .body(Full::new(bytes::Bytes::new()))?,
            )
            .await?;
        let (parts, body) = response.into_parts();
        if let Some(length) = parts.headers.get("content-length") {
            ensure!(
                length.to_str()?.parse::<u64>()? <= schema::MAX_BODY as u64,
                "Docker body limit"
            );
        }
        let body = Limited::new(body, schema::MAX_BODY)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("read bounded Docker body: {e}"))?
            .to_bytes()
            .to_vec();
        Ok((parts.status.as_u16(), parts.headers, body))
    };
    tokio::pin!(exchange, connection);
    // The connection driver belongs to this exchange; cancellation drops both futures/socket.
    tokio::select! {biased;result=&mut exchange=>result,result=&mut connection=>{
        // A complete framed body can precede a Unix proxy shutdown error. Consume
        // the buffered exchange first; partial/malformed bodies still fail closed.
        match exchange.await {Ok(response)=>Ok(response),Err(error)=>{result.context("Docker connection driver")?;Err(error)}}
    }}
}
fn header<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Result<&'a str> {
    ensure!(
        headers.get_all(name).iter().count() == 1,
        "expected exactly one {name} header"
    );
    Ok(headers.get(name).unwrap().to_str()?)
}
fn ping(status: u16, headers: &hyper::HeaderMap, body: &[u8], head: bool) -> Result<Value> {
    ensure!(status == 200, "Docker ping returned HTTP {status}");
    let api = header(headers, "api-version")?;
    api_version(api)?;
    ensure!(head || body == b"OK", "invalid Docker ping body");
    let os = headers.get("ostype").map(|v| v.to_str()).transpose()?;
    let experimental = headers
        .get("docker-experimental")
        .map(|v| match v.to_str()? {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => anyhow::bail!("invalid Docker-Experimental header"),
        })
        .transpose()?;
    Ok(json!({"api_version":api,"os_type":os,"experimental":experimental}))
}
fn response(
    request: &Request,
    status: u16,
    headers: &hyper::HeaderMap,
    body: &[u8],
) -> Result<Value> {
    if request.operation == "ping" {
        return ping(status, headers, body, false);
    }
    ensure!(status == 200, "Docker returned HTTP {status}");
    let mime = header(headers, "content-type")?
        .split(';')
        .next()
        .unwrap()
        .trim();
    ensure!(
        mime.eq_ignore_ascii_case("application/json"),
        "expected Docker application/json response"
    );
    for encoding in headers.get_all("content-encoding") {
        ensure!(
            encoding.to_str()?.eq_ignore_ascii_case("identity"),
            "unsupported Docker Content-Encoding"
        );
    }
    schema::parse(&request.operation, body)
}
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let endpoint = Endpoint::parse(&ctx.remote_addr)?;
    let mut preferred = DEFAULT_API_VERSION.to_owned();
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    if let Some(params) = &ctx.startup_params {
        preferred = params
            .get_optional_string("api_version")?
            .unwrap_or_else(|| DEFAULT_API_VERSION.into());
        timeout_secs = params
            .get_optional_u64("request_timeout_secs")?
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
    }
    let preferred_version = api_version(&preferred)?;
    ensure!(
        (api_version("1.24")?..=api_version(DEFAULT_API_VERSION)?).contains(&preferred_version),
        "api_version must be 1.24..1.47"
    );
    ensure!(
        (1..=MAX_TIMEOUT_SECS).contains(&timeout_secs),
        "request_timeout_secs must be 1..30"
    );
    let timeout = Duration::from_secs(timeout_secs);
    let (status, headers, body) = tokio::time::timeout(timeout, fetch(&endpoint, "HEAD", "/_ping"))
        .await
        .context("Docker negotiation deadline")??;
    let negotiated = ping(status, &headers, &body, true)?;
    let maximum = api_version(negotiated["api_version"].as_str().unwrap())?;
    ensure!(
        maximum >= api_version("1.24")?,
        "Docker daemon API is below 1.24"
    );
    let selected = maximum.min(preferred_version);
    let version = format!("{}.{}", selected.0, selected.1);
    let remote = match &endpoint {
        Endpoint::Tcp { host, port, .. } => {
            tokio::time::timeout(timeout, tokio::net::lookup_host((host.as_str(), *port)))
                .await
                .context("Docker resolution deadline")??
                .next()
                .context("Docker resolved no addresses")?
        }
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        Endpoint::Unix(_) => "0.0.0.0:0".parse()?,
    };
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
                &DockerClientProtocol,
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
                                .warn("Docker handler followup limit reached");
                            continue;
                        }
                        if actions_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&handler_ctx.status_tx)).warn(format!("Docker handler: {e}"))
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
            &endpoint,
            &version,
            negotiated,
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
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Docker ended: {e}"));
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
#[allow(clippy::too_many_arguments)]
async fn session(
    ctx: &ConnectContext,
    endpoint: &Endpoint,
    version: &str,
    negotiated: Value,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    events
        .try_send((
            Event::new(
                &actions::CONNECTED_EVENT,
                json!({"endpoint":endpoint.name(),"api_version":version,"daemon":negotiated}),
            ),
            0,
        ))
        .context("Docker event queue full")?;
    loop {
        let (action, depth, command) = tokio::select! {command=external.recv()=>match command {Some(c)=>(c.action.clone(),0,Some(c)),None=>return Ok(())},action=internal.recv()=>match action {Some((a,d))=>(a,d,None),None=>return Ok(())}};
        if action["type"] == "disconnect" {
            if let Some(c) = command {
                command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
            }
            return Ok(());
        }
        let request = match request(&action, version) {
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
                    Log::new(Some(&ctx.status_tx)).warn(format!("Docker action rejected: {e}"));
                }
                continue;
            }
        };
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Docker",
                    None,
                    "injected_action",
                    action.clone(),
                    vec![json!({"accepted":true})],
                )
                .await;
            command_support::reply(
                c,
                Ok(ClientSendOutcome::Executed {
                    detail: "read-only request accepted; result follows as an event".into(),
                }),
            );
        }
        let result = {
            let exchange = tokio::time::timeout(timeout, fetch(endpoint, "GET", &request.path));
            tokio::pin!(exchange);
            loop {
                tokio::select! {biased;command=external.recv()=>{let Some(c)=command else{return Ok(())};if c.action["type"]=="disconnect" {command_support::reply(c,Ok(ClientSendOutcome::Disconnected));return Ok(());}command_support::reply(c,Ok(ClientSendOutcome::Rejected{error:"Docker request pending; retry after its event".into()}));},result=&mut exchange=>break result.context("whole Docker request deadline").and_then(|r|r)}
            }
        };
        let event = match result {
            Ok((status, headers, body)) => {
                if status != 200 {
                    let error =
                        schema::error(&body).unwrap_or_else(|e| format!("HTTP {status}; {e:#}"));
                    Event::new(
                        &actions::ERROR_EVENT,
                        json!({"request":action,"api_version":version,"status":status,"error":error,"category":"http"}),
                    )
                } else {
                    match response(&request, status, &headers, &body) {
                        Ok(data) => Event::new(
                            &actions::RESPONSE_EVENT,
                            json!({"request":action,"api_version":version,"operation":request.operation,"status":status,"data":data}),
                        ),
                        Err(e) => Event::new(
                            &actions::ERROR_EVENT,
                            json!({"request":action,"api_version":version,"status":status,"error":format!("{e:#}"),"category":"schema"}),
                        ),
                    }
                }
            }
            Err(e) => Event::new(
                &actions::ERROR_EVENT,
                json!({"request":action,"api_version":version,"status":null,"error":format!("{e:#}"),"category":"transport"}),
            ),
        };
        events
            .try_send((event, depth))
            .context("Docker event queue full; disconnecting bounded client")?;
    }
}
