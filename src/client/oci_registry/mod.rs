pub mod actions;
pub mod api;
use crate::{
    client::{command_support, llm_budget::call_llm_for_client},
    logging::emit::Log,
    protocol::{ConnectContext, Event, EventType},
    state::{
        client_handles::{ClientCommand, ClientSendOutcome},
        AccessLogOwner, ClientStatus,
    },
};
pub use actions::OciRegistryClientProtocol;
use anyhow::{ensure, Context, Result};
use hyper::{header::HeaderValue, HeaderMap};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
pub const DEFAULT_TIMEOUT_SECS: u64 = 15;
pub const QUEUE_CAPACITY: usize = 8;
pub const MAX_FOLLOWUPS: u8 = 4;
#[cfg(not(target_arch = "wasm32"))]
type HttpClient = reqwest::Client;
#[cfg(target_arch = "wasm32")]
type HttpClient = ();
type HandlerEvent = (Event, u8);
type HandlerAction = (Value, u8);
#[cfg(not(target_arch = "wasm32"))]
async fn fetch(
    client: &HttpClient,
    url: &str,
    method: &str,
    credential: Option<&HeaderValue>,
    accept: &str,
) -> Result<(u16, HeaderMap, Vec<u8>)> {
    let mut request = client
        .request(method.parse()?, url)
        .header("Accept", accept)
        .header("Accept-Encoding", "identity")
        .header("Connection", "close");
    if let Some(c) = credential {
        request = request.header("Authorization", c.clone());
    }
    let mut response = request.send().await?;
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    api::headers(&headers)?;
    if method != "HEAD" {
        ensure!(
            response
                .content_length()
                .is_none_or(|n| n <= api::MAX_BODY as u64),
            "OCI body bound"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= api::MAX_BODY.saturating_sub(body.len()),
            "OCI body bound"
        );
        body.extend_from_slice(&chunk);
    }
    Ok((status, headers, body))
}
#[cfg(target_arch = "wasm32")]
async fn fetch(
    _client: &HttpClient,
    url: &str,
    method: &str,
    credential: Option<&HeaderValue>,
    accept: &str,
) -> Result<(u16, HeaderMap, Vec<u8>)> {
    use http_body_util::{BodyExt, Full, Limited};
    let target = crate::client::http_fetch::transport::parse_http_url(url)?;
    let io = tokio::net::TcpStream::connect((target.host.as_str(), target.port)).await?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(io)).await?;
    let exchange = async {
        let mut request = hyper::Request::builder()
            .method(method)
            .uri(&target.path_and_query)
            .header("Host", &target.authority)
            .header("Accept", accept)
            .header("Accept-Encoding", "identity")
            .header("Connection", "close");
        if let Some(c) = credential {
            request = request.header("Authorization", c.clone());
        }
        let response = sender
            .send_request(request.body(Full::new(bytes::Bytes::new()))?)
            .await?;
        let (parts, body) = response.into_parts();
        api::headers(&parts.headers)?;
        // `Limited`'s error is a boxed `dyn Error`, which `?` cannot convert into anyhow on
        // every target (the wasm32 build has no other conversion in scope); say it explicitly.
        let body = Limited::new(body, api::MAX_BODY)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("OCI response body: {e}"))?
            .to_bytes();
        Ok((parts.status.as_u16(), parts.headers, body.to_vec()))
    };
    tokio::pin!(exchange, connection);
    tokio::select! {r=&mut exchange=>r,r=&mut connection=>{let _=r;exchange.await}}
}
fn emit(
    events: &mpsc::Sender<HandlerEvent>,
    event: &'static EventType,
    mut data: Value,
    depth: u8,
    secrets: &[String],
) -> Result<()> {
    api::redact_payload(&mut data, secrets);
    ensure!(
        crate::utils::json_budget::within_budget(
            &data,
            api::MAX_RETAINED,
            api::MAX_NODES,
            api::MAX_DEPTH + 8
        ),
        "OCI converted event budget"
    );
    events
        .try_send((Event::new(event, data), depth))
        .context("OCI event queue full")
}
fn parsed_result(
    outcome: api::Outcome,
    last: &mut Option<api::Challenge>,
) -> (&'static EventType, Value) {
    match outcome {
        api::Outcome::Result(v) => (&actions::RESULT_EVENT, v),
        api::Outcome::Failure(v) => (&actions::FAILURE_EVENT, v),
        api::Outcome::Challenge(c, mut v) => {
            v["challenge"] = c.shown();
            *last = Some(c);
            (&actions::CHALLENGE_EVENT, v)
        }
    }
}
pub async fn connect(mut ctx: ConnectContext) -> Result<SocketAddr> {
    let endpoint = if ctx.remote_addr.contains("://") {
        ctx.remote_addr.clone()
    } else {
        format!("http://{}", ctx.remote_addr)
    };
    let origin = api::origin(&endpoint)?;
    let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
    let mut trusted_origin = None;
    let mut secret = None;
    if let Some(p) = &ctx.startup_params {
        timeout_secs = p
            .get_optional_u64("request_timeout_secs")
            .map_err(|_| anyhow::anyhow!("OCI timeout requires integer"))?
            .unwrap_or(timeout_secs);
        trusted_origin = p
            .get_optional_string("trusted_token_origin")
            .map_err(|_| anyhow::anyhow!("OCI trusted token origin requires string"))?
            .map(|s| api::origin(&s))
            .transpose()?;
        secret = p
            .get_optional_string("token")
            .map_err(|_| anyhow::anyhow!("OCI token requires string"))?;
    }
    ensure!(
        (1..=30).contains(&timeout_secs),
        "OCI request_timeout_secs must be1..30"
    );
    if let Some(s) = &secret {
        api::token(s)?;
        ensure!(
            api::secure_credentials(&origin),
            "OCI credentials require verified HTTPS or numeric loopback HTTP"
        );
    }
    if let Some(o) = &trusted_origin {
        ensure!(
            api::secure_credentials(o),
            "OCI trusted token origin must use verified HTTPS or numeric loopback HTTP"
        );
    }
    ctx.startup_params = None;
    let timeout = Duration::from_secs(timeout_secs);
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
        .user_agent("NetGet-OCI/1.0")
        .build()?;
    #[cfg(target_arch = "wasm32")]
    let client = ();
    let startup = async {
        let host = origin.host_str().unwrap().trim_matches(['[', ']']);
        let remote = tokio::net::lookup_host((host, origin.port_or_known_default().unwrap()))
            .await?
            .next()
            .context("OCI origin resolved no address")?;
        let api::Action::Request(probe) =
            api::action(&json!({"type":"oci_request","operation":"probe"}))?
        else {
            unreachable!()
        };
        let (status, h, b) = fetch(
            &client,
            &format!("{}/v2/", origin.origin().ascii_serialization()),
            "GET",
            None,
            "application/json",
        )
        .await?;
        ensure!(matches!(status, 200 | 401), "OCI startup probe refused");
        let result = api::response(&probe, status, &h, &b)?;
        Ok::<_, anyhow::Error>((remote, result))
    };
    let (remote, probe) = tokio::time::timeout(timeout, startup)
        .await
        .context("OCI startup deadline")??;
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
                &OciRegistryClientProtocol,
                &handler_ctx.status_tx,
            )
            .await
            {
                Ok(result) => {
                    if let Some(m) = result.memory_updates {
                        handler_ctx
                            .state
                            .set_memory_for_client(handler_ctx.client_id, m)
                            .await;
                    }
                    for action in result.actions {
                        if depth >= MAX_FOLLOWUPS && action["type"] != "disconnect" {
                            crate::utils::json_budget::drop_iteratively(action);
                            Log::new(Some(&handler_ctx.status_tx))
                                .warn("OCI handler followup limit reached");
                            continue;
                        }
                        if actions_tx.send((action, depth + 1)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(_) => Log::new(Some(&handler_ctx.status_tx))
                    .warn("OCI handler failed; credential diagnostics hidden"),
            }
        }
    });
    let handler_abort = handler.abort_handle();
    ctx.state.register_client_task(ctx.client_id, handler).await;
    let task_ctx = ctx.clone();
    let session = tokio::spawn(async move {
        let result = session(
            &task_ctx,
            &client,
            origin,
            trusted_origin,
            secret,
            probe,
            timeout,
            external,
            actions_rx,
            events_tx,
        )
        .await;
        handler_abort.abort();
        task_ctx
            .state
            .update_client_status(
                task_ctx.client_id,
                if result.is_ok() {
                    ClientStatus::Disconnected
                } else {
                    Log::new(Some(&task_ctx.status_tx)).warn("OCI bounded session failure");
                    ClientStatus::Error("OCI bounded session failure".into())
                },
            )
            .await;
        task_ctx
            .state
            .remove_client_handle(task_ctx.client_id)
            .await;
        let _ = task_ctx.status_tx.send("__UPDATE_UI__".into());
    });
    ctx.state.register_client_task(ctx.client_id, session).await;
    Ok(remote)
}
struct Credential {
    header: HeaderValue,
    deadline: Option<tokio::time::Instant>,
}
struct SessionState {
    credential: Option<Credential>,
    token_secret: Option<String>,
    password: Option<String>,
    basic_secret: Option<String>,
    challenge: Option<api::Challenge>,
}
impl SessionState {
    fn secrets(&self) -> Vec<String> {
        self.token_secret
            .iter()
            .chain(self.password.iter())
            .chain(self.basic_secret.iter())
            .cloned()
            .collect()
    }
    fn header(&mut self) -> Option<&HeaderValue> {
        if self
            .credential
            .as_ref()
            .is_some_and(|c| c.deadline.is_some_and(|t| t <= tokio::time::Instant::now()))
        {
            self.credential = None;
        }
        self.credential.as_ref().map(|c| &c.header)
    }
}
#[allow(clippy::too_many_arguments)]
async fn session(
    ctx: &ConnectContext,
    client: &HttpClient,
    origin: url::Url,
    trusted_origin: Option<url::Url>,
    secret: Option<String>,
    probe: api::Outcome,
    timeout: Duration,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<HandlerAction>,
    events: mpsc::Sender<HandlerEvent>,
) -> Result<()> {
    let mut state = SessionState {
        credential: secret
            .as_deref()
            .map(api::token)
            .transpose()?
            .map(|header| Credential {
                header,
                deadline: None,
            }),
        token_secret: secret,
        password: None,
        basic_secret: None,
        challenge: None,
    };
    let (_, probe) = parsed_result(probe, &mut state.challenge);
    emit(
        &events,
        &actions::CONNECTED_EVENT,
        json!({"origin":origin.origin().ascii_serialization(),"data":probe,"token_present":state.credential.is_some(),"authentication_verified":false}),
        0,
        &state.secrets(),
    )?;
    loop {
        let (value, depth, command) = tokio::select! {c=external.recv()=>{let Some(mut c)=c else{return Ok(())};(std::mem::take(&mut c.action),0,Some(c))},a=internal.recv()=>{let Some((v,d))=a else{return Ok(())};(v,d,None)}};
        if !api::within_budget(&value) {
            crate::utils::json_budget::drop_iteratively(value);
            if let Some(c) = command {
                command_support::reply(
                    c,
                    Ok(ClientSendOutcome::Rejected {
                        error: "OCI action budget refusal".into(),
                    }),
                );
            }
            continue;
        }
        let parsed = api::action(&value);
        let shown = if parsed.is_ok() {
            json!({"type":value["type"]})
        } else {
            json!({})
        };
        crate::utils::json_budget::drop_iteratively(value);
        let action = match parsed {
            Ok(a) => a,
            Err(_) => {
                if let Some(c) = command {
                    command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: "invalid selected OCI action".into(),
                        }),
                    );
                } else {
                    emit(
                        &events,
                        &actions::ERROR_EVENT,
                        json!({"category":"action","error":"invalid selected OCI action"}),
                        depth,
                        &state.secrets(),
                    )?;
                }
                continue;
            }
        };
        if matches!(action, api::Action::Disconnect) {
            if let Some(c) = command {
                command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
            }
            return Ok(());
        }
        // Validate trust and credential shape before acknowledging or issuing any request.
        let prepared = prepare(action, &origin, trusted_origin.as_ref(), &mut state);
        let prepared = match prepared {
            Ok(p) => p,
            Err(_) => {
                if let Some(c) = command {
                    command_support::reply(
                        c,
                        Ok(ClientSendOutcome::Rejected {
                            error: "OCI authentication/trust refusal".into(),
                        }),
                    );
                } else {
                    emit(
                        &events,
                        &actions::ERROR_EVENT,
                        json!({"category":"action","error":"OCI authentication/trust refusal"}),
                        depth,
                        &state.secrets(),
                    )?;
                }
                continue;
            }
        };
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "oci-registry",
                    None,
                    "injected_action",
                    shown,
                    vec![json!({"pending":true})],
                )
                .await;
        }
        let result = {
            let deadline = tokio::time::Instant::now() + timeout;
            let pending = tokio::time::timeout(timeout, perform(client, prepared, &mut state));
            tokio::pin!(pending);
            let result = loop {
                if tokio::time::Instant::now() >= deadline {
                    break Err(anyhow::anyhow!("OCI whole request deadline"));
                }
                tokio::select! {biased;
                    c=external.recv()=>{let Some(mut c)=c else{return Ok(())};let value=std::mem::take(&mut c.action);let disconnect=matches!(api::action(&value),Ok(api::Action::Disconnect));crate::utils::json_budget::drop_iteratively(value);if disconnect{command_support::reply(c,Ok(ClientSendOutcome::Disconnected));return Ok(());}command_support::reply(c,Ok(ClientSendOutcome::Rejected{error:"OCI request pending; wait for native result".into()}));},
                    r=&mut pending=>break r.map_err(anyhow::Error::from),
                }
            };
            result
        };
        match result {
            Ok(Ok((event, data))) => {
                if let Some(c) = command {
                    command_support::reply(c, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                }
                emit(&events, event, data, depth, &state.secrets())?;
            }
            _ => {
                if let Some(c) = command {
                    command_support::reply(
                        c,
                        Err(anyhow::anyhow!(
                            "OCI no complete native response; backend outcome unknown"
                        )),
                    );
                }
                emit(
                    &events,
                    &actions::ERROR_EVENT,
                    json!({"category":"transport_or_schema","error":"OCI complete native response refused or deadline reached"}),
                    depth,
                    &state.secrets(),
                )?;
            }
        }
    }
}
enum Prepared {
    Request(api::Request, String, Option<HeaderValue>),
    Auth(String, Option<HeaderValue>),
    Local(Value),
}
fn prepare(
    action: api::Action,
    origin: &url::Url,
    trusted_origin: Option<&url::Url>,
    state: &mut SessionState,
) -> Result<Prepared> {
    Ok(match action {
        api::Action::Request(r) => {
            let h = state.header().cloned();
            if h.is_some() {
                ensure!(
                    api::secure_credentials(origin),
                    "OCI credential transport refusal"
                );
            }
            let url = format!("{}{}", origin.origin().ascii_serialization(), r.path);
            Prepared::Request(r, url, h)
        }
        api::Action::Authenticate { username, password } => {
            let c = state
                .challenge
                .as_ref()
                .context("OCI authenticate requires native401 challenge")?;
            api::trusted(c, origin, trusted_origin)?;
            let url = c.token_url().to_string();
            let header = if let (Some(u), Some(p)) = (username, password) {
                use base64::Engine;
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
                state.password = Some(p);
                state.basic_secret = Some(encoded.clone());
                let mut h: HeaderValue = format!("Basic {encoded}").parse()?;
                h.set_sensitive(true);
                Some(h)
            } else {
                None
            };
            Prepared::Auth(url, header)
        }
        api::Action::SetToken(s) => {
            ensure!(
                api::secure_credentials(origin),
                "OCI credential transport refusal"
            );
            state.credential = Some(Credential {
                header: api::token(&s)?,
                deadline: None,
            });
            state.token_secret = Some(s);
            Prepared::Local(
                json!({"operation":"set_token","token_present":true,"authentication_verified":false}),
            )
        }
        api::Action::ClearToken => {
            state.credential = None;
            Prepared::Local(
                json!({"operation":"clear_token","token_present":false,"authentication_verified":false}),
            )
        }
        api::Action::Disconnect => unreachable!(),
    })
}
async fn perform(
    client: &HttpClient,
    prepared: Prepared,
    state: &mut SessionState,
) -> Result<(&'static EventType, Value)> {
    match prepared {
        Prepared::Local(v) => Ok((&actions::AUTH_EVENT, v)),
        Prepared::Request(r, url, h) => {
            let (status, headers, body) = fetch(
                client,
                &url,
                r.method,
                h.as_ref(),
                if r.operation.starts_with("manifest") {
                    api::ACCEPT
                } else if r.operation.starts_with("blob") {
                    "*/*"
                } else {
                    "application/json"
                },
            )
            .await?;
            let result = api::response(&r, status, &headers, &body)?;
            if status == 401 {
                state.credential = None;
            }
            Ok(parsed_result(result, &mut state.challenge))
        }
        Prepared::Auth(url, h) => {
            let (status, headers, body) =
                fetch(client, &url, "GET", h.as_ref(), "application/json").await?;
            let issued = api::issued_token(status, &headers, &body)?;
            state.credential = Some(Credential {
                header: api::token(&issued.secret)?,
                deadline: Some(tokio::time::Instant::now() + issued.ttl),
            });
            state.token_secret = Some(issued.secret);
            Ok((
                &actions::AUTH_EVENT,
                json!({"operation":"authenticate","data":issued.metadata}),
            ))
        }
    }
}
