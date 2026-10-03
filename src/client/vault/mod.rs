pub mod actions;
pub mod api;
use crate::{
    client::{command_support, llm_budget::call_llm_for_client},
    logging::emit::Log,
    protocol::{ConnectContext, Event},
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, ClientStatus,
    },
};
pub use actions::VaultClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const DEFAULT_KV_MOUNT: &str = "secret";
pub const DEFAULT_AUTH_MOUNT: &str = "userpass";
pub const MAX_FOLLOWUPS: u8 = 4;
pub const QUEUE_CAPACITY: usize = 8;
#[cfg(not(target_arch = "wasm32"))]
type HttpClient = reqwest::Client;
#[cfg(target_arch = "wasm32")]
type HttpClient = ();
#[cfg(not(target_arch = "wasm32"))]
async fn fetch(
    client: &HttpClient,
    origin: &str,
    request: &api::Request,
    credential: Option<&hyper::header::HeaderValue>,
) -> Result<(u16, hyper::HeaderMap, Vec<u8>)> {
    let mut builder = client
        .request(request.method.parse()?, format!("{origin}{}", request.path))
        .header("Accept", "application/json")
        .header("Accept-Encoding", "identity")
        .header("X-Vault-Request", "true")
        .header("Connection", "close");
    if let Some(token) = credential {
        builder = builder.header("X-Vault-Token", token.clone());
    }
    if !request.body.is_empty() {
        builder = builder
            .header("Content-Type", "application/json")
            .body(request.body.clone());
    }
    let mut response = builder.send().await.context("send Vault request")?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    if let Some(length) = response.content_length() {
        ensure!(length <= api::MAX_BODY as u64, "Vault body limit");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read Vault body")? {
        ensure!(
            body.len().saturating_add(chunk.len()) <= api::MAX_BODY,
            "Vault body limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok((status, headers, body))
}
#[cfg(target_arch = "wasm32")]
async fn fetch(
    _client: &HttpClient,
    origin: &str,
    request: &api::Request,
    credential: Option<&hyper::header::HeaderValue>,
) -> Result<(u16, hyper::HeaderMap, Vec<u8>)> {
    use http_body_util::{BodyExt, Full, Limited};
    let target =
        crate::client::http_fetch::transport::parse_http_url(&format!("{origin}{}", request.path))?;
    let stream = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
    let exchange = async {
        let mut builder = hyper::Request::builder()
            .method(request.method)
            .uri(target.path_and_query.as_str())
            .header("Host", target.authority.as_str())
            .header("Accept", "application/json")
            .header("Accept-Encoding", "identity")
            .header("X-Vault-Request", "true")
            .header("Connection", "close");
        if let Some(token) = credential {
            builder = builder.header("X-Vault-Token", token.clone());
        }
        if !request.body.is_empty() {
            builder = builder.header("Content-Type", "application/json");
        }
        let response = sender
            .send_request(builder.body(Full::new(bytes::Bytes::copy_from_slice(&request.body)))?)
            .await?;
        let (parts, body) = response.into_parts();
        if let Some(length) = parts.headers.get("content-length") {
            ensure!(
                length.to_str()?.parse::<u64>()? <= api::MAX_BODY as u64,
                "Vault body limit"
            );
        }
        let body = Limited::new(body, api::MAX_BODY)
            .collect()
            .await?
            .to_bytes();
        Ok((parts.status.as_u16(), parts.headers, body.to_vec()))
    };
    tokio::pin!(exchange, connection);
    tokio::select! {result=&mut exchange=>result,result=&mut connection=>{let _=result;exchange.await}}
}
fn decode(headers: &hyper::HeaderMap, body: &[u8]) -> Result<Value> {
    let mime = headers
        .get("content-type")
        .context("missing Vault Content-Type")?
        .to_str()?
        .split(';')
        .next()
        .unwrap()
        .trim();
    ensure!(
        mime.eq_ignore_ascii_case("application/json"),
        "Vault response requires application/json"
    );
    for encoding in headers.get_all("content-encoding") {
        ensure!(
            encoding.to_str()?.eq_ignore_ascii_case("identity"),
            "unsupported Vault Content-Encoding"
        );
    }
    api::json(body)
}
fn success(operation: &str, status: u16) -> bool {
    status == 200 || operation == "health" && [429, 472, 473, 474, 501, 503, 530].contains(&status)
}
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let address = if ctx.remote_addr.contains("://") {
        ctx.remote_addr.clone()
    } else {
        format!("http://{}", ctx.remote_addr)
    };
    let url = url::Url::parse(&address).context("invalid Vault endpoint")?;
    ensure!(
        ["http", "https"].contains(&url.scheme()) && url.host_str().is_some(),
        "Vault endpoint requires HTTP(S)"
    );
    ensure!(
        url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Vault endpoint must be an origin without credentials/path/query"
    );
    let origin = url.origin().ascii_serialization();
    let mut mount = DEFAULT_KV_MOUNT.to_string();
    let mut auth_mount = DEFAULT_AUTH_MOUNT.to_string();
    let mut credential = None;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    if let Some(params) = &ctx.startup_params {
        mount = params.get_optional_string("kv_mount")?.unwrap_or(mount);
        auth_mount = params
            .get_optional_string("auth_mount")?
            .unwrap_or(auth_mount);
        timeout_secs = params
            .get_optional_u64("request_timeout_secs")?
            .unwrap_or(timeout_secs);
        if let Some(value) = params.get_optional_string("token")? {
            credential = Some(api::token(&value)?);
        }
    }
    api::mount(&mount)?;
    api::mount(&auth_mount)?;
    ensure!(
        (1..=30).contains(&timeout_secs),
        "request_timeout_secs must be1..30"
    );
    let timeout = Duration::from_secs(timeout_secs);
    #[cfg(target_arch = "wasm32")]
    crate::client::http_fetch::transport::parse_http_url(&origin)?;
    let host = url.host_str().unwrap().trim_matches(['[', ']']);
    let remote = tokio::time::timeout(
        timeout,
        tokio::net::lookup_host((host, url.port_or_known_default().unwrap())),
    )
    .await
    .context("Vault resolution deadline")??
    .next()
    .context("Vault resolved no addresses")?;
    #[cfg(not(target_arch = "wasm32"))]
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .pool_max_idle_per_host(0)
        .http1_only()
        .user_agent("NetGet-Vault/1.0")
        .build()?;
    #[cfg(target_arch = "wasm32")]
    let client = ();
    // Seal status is public. A configured startup token is not yet authenticated.
    let probe = api::request(
        &json!({"type":"vault_request","operation":"seal_status"}),
        &mount,
        &auth_mount,
    )?;
    let startup = async {
        let (status, headers, body) =
            tokio::time::timeout(timeout, fetch(&client, &origin, &probe, None))
                .await
                .context("Vault startup deadline")??;
        ensure!(status == 200, "Vault seal probe HTTP{status}");
        api::parse("seal_status", decode(&headers, &body)?)
    }
    .await;
    let (mut seal, _) = startup.map_err(|error| {
        let secrets = credential
            .as_ref()
            .map(|token| token.to_str().unwrap().to_owned())
            .into_iter()
            .collect::<Vec<_>>();
        anyhow::anyhow!("{}", api::redact_text(&format!("{error:#}"), &secrets))
    })?;
    if let Some(token) = &credential {
        api::redact_response(&mut seal, "seal_status", &[token.to_str()?.into()]);
    }
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
                &VaultClientProtocol,
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
                                .warn("Vault handler followup limit reached");
                            continue;
                        }
                        if actions_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(_) => Log::new(Some(&handler_ctx.status_tx))
                    .warn("Vault event handler failed; credential diagnostics hidden"),
            }
        }
    });
    let handler_abort = handler.abort_handle();
    ctx.state.register_client_task(ctx.client_id, handler).await;
    let task_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &task_ctx,
            &client,
            &origin,
            &mount,
            &auth_mount,
            credential,
            seal,
            timeout,
            external,
            actions_rx,
            events_tx,
        )
        .await;
        handler_abort.abort();
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(_) => {
                Log::new(Some(&task_ctx.status_tx)).warn("Vault ended: bounded session failure");
                ClientStatus::Error("Vault bounded session failure".into())
            }
        };
        task_ctx
            .state
            .update_client_status(task_ctx.client_id, status)
            .await;
        task_ctx
            .state
            .remove_client_handle(task_ctx.client_id)
            .await;
        let _ = task_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, task).await;
    Ok(remote)
}
#[allow(clippy::too_many_arguments)]
async fn session(
    ctx: &ConnectContext,
    client: &HttpClient,
    origin: &str,
    mount: &str,
    auth_mount: &str,
    mut credential: Option<hyper::header::HeaderValue>,
    seal: Value,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    // Keep only the latest login password for redacting later reflected metadata;
    // it is bounded by MAX_TEXT and is never used for automatic reauthentication.
    let mut login_password: Option<String> = None;
    events.try_send((Event::new(&actions::CONNECTED_EVENT,json!({"origin":origin,"kv_mount":mount,"auth_mount":auth_mount,"token_present":credential.is_some(),"authentication_verified":false,"seal":seal})),0)).context("Vault event queue full")?;
    loop {
        let (action, depth, command) = tokio::select! {command=external.recv()=>match command {Some(c)=>(c.action.clone(),0,Some(c)),None=>return Ok(())},action=internal.recv()=>match action {Some((a,d))=>(a,d,None),None=>return Ok(())}};
        if action["type"] == "disconnect" {
            if let Some(c) = command {
                command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
            }
            return Ok(());
        }
        let mut request = match api::request(&action, mount, auth_mount) {
            Ok(r) => r,
            Err(_) => {
                if let Some(c) = command {
                    command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: "invalid selected Vault action".into(),
                        }),
                    );
                } else {
                    Log::new(Some(&ctx.status_tx)).warn("Vault rejected invalid selected action");
                }
                continue;
            }
        };
        let mut secrets = Vec::new();
        if let Some(token) = &credential {
            secrets.push(token.to_str()?.into());
        }
        if let Some(password) = &login_password {
            secrets.push(password.clone());
        }
        if request.operation == "login" {
            credential = None;
            login_password = Some(action["password"].as_str().unwrap().into());
            secrets.push(login_password.as_ref().unwrap().clone());
        }
        api::redact_request(&mut request.redacted, &secrets);
        // Preserve the validated action discriminants after redacting free-form values.
        request.redacted["type"] = action["type"].clone();
        if let Some(operation) = action.get("operation") {
            request.redacted["operation"] = operation.clone();
        }
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "Vault",
                    None,
                    "injected_action",
                    request.redacted.clone(),
                    vec![json!({"accepted":true})],
                )
                .await;
            command_support::reply(
                c,
                Ok(ClientSendOutcome::Executed {
                    detail: "Vault action accepted; typed result follows as an event".into(),
                }),
            );
        }
        if request.operation == "clear_token" {
            credential = None;
            login_password = None;
            events.try_send((Event::new(&actions::AUTH_EVENT,json!({"request":request.redacted,"operation":"clear_token","token_present":false,"authentication_verified":false,"status":null,"auth":null})),depth)).context("Vault event queue full")?;
            continue;
        }
        let result = {
            let exchange = tokio::time::timeout(
                timeout,
                fetch(client, origin, &request, credential.as_ref()),
            );
            tokio::pin!(exchange);
            loop {
                tokio::select! {biased;command=external.recv()=>{let Some(c)=command else{return Ok(())};if c.action["type"]=="disconnect" {command_support::reply(c,Ok(ClientSendOutcome::Disconnected));return Ok(());}command_support::reply(c,Ok(ClientSendOutcome::Rejected{error:"Vault request pending; retry after its event".into()}));},result=&mut exchange=>break result.context("whole Vault request deadline").and_then(|r|r)}
            }
        };
        let event = match result {
            Ok((status, headers, body)) => {
                let decoded = decode(&headers, &body);
                if let Ok(v) = &decoded {
                    if let Some(token) = v.pointer("/auth/client_token").and_then(Value::as_str) {
                        secrets.push(token.into());
                    }
                }
                let diagnostics = |error: anyhow::Error| vec![format!("{error:#}")];
                let result = decoded.map_err(diagnostics).and_then(|value| {
                    if success(&request.operation, status) {
                        api::parse(&request.operation, value).map_err(diagnostics)
                    } else {
                        match api::errors(value) {
                            Ok(errors) => Err(errors),
                            Err(error) => Err(diagnostics(error)),
                        }
                    }
                });
                match result {
                    Ok((mut data, token)) => {
                        let token_type = data["data"]["token_type"].clone();
                        api::redact_response(&mut data, &request.operation, &secrets);
                        if request.operation == "login" {
                            // This field has already been validated as service/batch.
                            data["data"]["token_type"] = token_type;
                            credential = token;
                            Event::new(
                                &actions::AUTH_EVENT,
                                json!({"request":request.redacted,"operation":"login","status":status,"token_present":credential.is_some(),"authentication_verified":true,"auth":data["data"],"request_id":data["request_id"],"warnings":data["warnings"]}),
                            )
                        } else {
                            Event::new(
                                &actions::RESPONSE_EVENT,
                                json!({"request":request.redacted,"operation":request.operation,"status":status,"data":data}),
                            )
                        }
                    }
                    Err(errors) => Event::new(
                        &actions::ERROR_EVENT,
                        json!({"request":request.redacted,"status":status,"category":if success(&request.operation,status){"schema"}else{"http"},"errors":api::redact_errors(&errors,&secrets),"token_present":credential.is_some()}),
                    ),
                }
            }
            Err(e) => Event::new(
                &actions::ERROR_EVENT,
                json!({"request":request.redacted,"status":null,"category":"transport","errors":[api::redact_text(&format!("{e:#}"),&secrets)],"token_present":credential.is_some()}),
            ),
        };
        events
            .try_send((event, depth))
            .context("Vault event queue full; bounded client disconnect")?;
    }
}
