pub mod actions;
pub mod codec;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::{
    accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS},
    connection::ConnectionId,
};
use crate::state::{
    server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo},
    AccessLogOwner,
};
use anyhow::{bail, ensure, Context, Result};
use codec::*;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
};
struct Config {
    secret: Vec<u8>,
    overrides: BTreeMap<IpAddr, Vec<u8>>,
    io_timeout: Duration,
    handler_timeout: Duration,
    llm_fallback: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientSecret {
    client_ip: IpAddr,
    shared_secret: String,
}
pub fn timeout(value: Option<u64>, default: u64) -> Result<Duration> {
    let seconds = value.unwrap_or(default);
    ensure!((1..=300).contains(&seconds), "timeout1..300seconds");
    Ok(Duration::from_secs(seconds))
}
pub struct TacacsServer;
impl TacacsServer {
    pub async fn spawn(mut ctx: SpawnContext) -> Result<SocketAddr> {
        let params = ctx
            .startup_params
            .as_ref()
            .context("TACACS shared_secret required")?;
        let secret = params.get_string("shared_secret")?;
        validate_secret(&secret)?;
        let mut overrides = BTreeMap::new();
        if let Some(values) = params.get_optional_array("client_secrets")? {
            ensure!(values.len() <= 64, "client secret capacity64");
            for value in values {
                ensure!(within_json_budget(value), "client secret JSON budget");
                let c: ClientSecret = serde_json::from_value(value.clone())?;
                validate_secret(&c.shared_secret)?;
                ensure!(
                    overrides
                        .insert(c.client_ip, c.shared_secret.into_bytes())
                        .is_none(),
                    "duplicate client IP secret"
                );
            }
        }
        let config = Arc::new(Config {
            secret: secret.into_bytes(),
            overrides,
            io_timeout: timeout(
                params.get_optional_u64("io_timeout_seconds")?,
                DEFAULT_IO_SECONDS,
            )?,
            handler_timeout: timeout(
                params.get_optional_u64("handler_timeout_seconds")?,
                DEFAULT_HANDLER_SECONDS,
            )?,
            llm_fallback: params
                .get_optional_bool("llm_fallback")?
                .unwrap_or(DEFAULT_LLM_FALLBACK),
        });
        ctx.startup_params = None;
        let listener = TcpListener::bind(
            ctx.socket_addr()
                .context("TACACS TCP bind address required")?,
        )
        .await?;
        let local = listener.local_addr()?;
        let state = ctx.state.clone();
        let sid = ctx.server_id;
        Log::new(Some(&ctx.status_tx)).info(format!("TACACS legacy listening on {local}"));
        state
            .clone()
            .spawn_server_task(sid, async move {
                let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
                loop {
                    let (socket, peer, permit) = match accept_bounded(
                        &listener,
                        &limiter,
                        b"",
                        "TACACS",
                        Some(&ctx.status_tx),
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let cid = ConnectionId::new(ctx.state.get_next_unified_id().await);
                    let now = crate::utils::clock::Instant::now();
                    ctx.state
                        .add_connection_to_server(
                            sid,
                            ConnectionState {
                                id: cid,
                                remote_addr: peer,
                                local_addr: local,
                                bytes_sent: 0,
                                bytes_received: 0,
                                packets_sent: 0,
                                packets_received: 0,
                                last_activity: now,
                                status: ConnectionStatus::Active,
                                status_changed_at: now,
                                protocol_info: ProtocolConnectionInfo::empty(),
                            },
                        )
                        .await;
                    let child = ctx.clone();
                    let cfg = config.clone();
                    ctx.state
                        .spawn_server_task(sid, async move {
                            let _permit = permit;
                            let secret = cfg.overrides.get(&peer.ip()).unwrap_or(&cfg.secret);
                            if session(&child, cid, socket, peer, secret, &cfg)
                                .await
                                .is_err()
                            {
                                decision(&child, cid, "fail_closed_session_error");
                            }
                            child
                                .state
                                .update_connection_status(sid, cid, ConnectionStatus::Closed)
                                .await;
                            child.state.remove_connection_from_server(sid, cid).await;
                            let _ = child.status_tx.send("__UPDATE_UI__".into());
                        })
                        .await;
                }
            })
            .await;
        Ok(local)
    }
}
fn decision(ctx: &SpawnContext, id: ConnectionId, tag: &str) {
    Log::new(Some(&ctx.status_tx)).info(format!("TACACS connection={id} decision={tag}"));
}
async fn read(
    ctx: &SpawnContext,
    id: ConnectionId,
    r: &mut OwnedReadHalf,
    secret: &[u8],
    cfg: &Config,
) -> Result<(Header, Vec<u8>)> {
    let packet = read_packet(r, secret, cfg.io_timeout).await?;
    ctx.state
        .update_connection_stats(
            ctx.server_id,
            id,
            Some((12 + packet.0.length) as u64),
            None,
            Some(1),
            None,
        )
        .await;
    Ok(packet)
}
async fn send(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut OwnedWriteHalf,
    h: Header,
    body: &[u8],
    secret: &[u8],
) -> Result<()> {
    let n = write_packet(w, h, body, secret).await?;
    ctx.state
        .update_connection_stats(ctx.server_id, id, None, Some(n as u64), None, Some(1))
        .await;
    Ok(())
}
fn error_body(kind: u8) -> Result<Vec<u8>> {
    match kind {
        1 => auth_reply_body(&AuthReply {
            status: AuthStatus::Error,
            no_echo: false,
            server_message: "Request could not be processed".into(),
            data: String::new(),
        }),
        2 => author_reply_body(&AuthorReply {
            status: AuthorStatus::Error,
            arguments: vec![],
            server_message: "Request could not be processed".into(),
            data: String::new(),
        }),
        3 => account_reply_body(&AccountReply {
            status: AccountStatus::Error,
            server_message: "Request could not be recorded".into(),
            data: String::new(),
        }),
        _ => Ok(vec![]),
    }
}
async fn fail(
    ctx: &SpawnContext,
    id: ConnectionId,
    w: &mut OwnedWriteHalf,
    h: Header,
    secret: &[u8],
    tag: &str,
) -> Result<()> {
    decision(ctx, id, tag);
    if let Ok(mut reply) = h.reply() {
        if !matches!(h.kind, 1..=3) {
            // RFC8907 section3.6: mirror unknown-type header, advance sequence, zero body.
            reply.flags = h.flags;
        }
        let body = error_body(h.kind)?;
        send(ctx, id, w, reply, &body, secret).await?;
    }
    Ok(())
}
async fn continuation(
    ctx: &SpawnContext,
    id: ConnectionId,
    r: &mut OwnedReadHalf,
    w: &mut OwnedWriteHalf,
    secret: &[u8],
    cfg: &Config,
    h: Header,
    status: AuthStatus,
    message: &str,
) -> Result<(Header, Continue)> {
    let reply = h.reply()?;
    let body = auth_reply_body(&AuthReply {
        status,
        no_echo: status == AuthStatus::GetPass,
        server_message: message.into(),
        data: String::new(),
    })?;
    send(ctx, id, w, reply, &body, secret).await?;
    let (next, body) = read(ctx, id, r, secret, cfg).await?;
    let valid = next.validate().is_ok()
        && next.version == h.version
        && next.kind == 1
        && next.session_id == h.session_id
        && reply.sequence.checked_add(1) == Some(next.sequence);
    if !valid {
        fail(ctx, id, w, next, secret, "fail_closed_continuation_header").await?;
        bail!("authentication correlation");
    }
    let response = match parse_continue(&body) {
        Ok(response) => response,
        Err(_) => {
            fail(ctx, id, w, next, secret, "fail_closed_continuation_body").await?;
            bail!("authentication continuation body");
        }
    };
    Ok((next, response))
}
async fn handler(
    ctx: &SpawnContext,
    id: ConnectionId,
    r: &mut OwnedReadHalf,
    cfg: &Config,
    event: Event,
    expected: &str,
) -> Result<Option<Value>> {
    let configured = ctx
        .state
        .get_event_handler_config(ctx.server_id)
        .await
        .is_some_and(|c| c.find_handler(event.id()).is_some());
    if !configured && !cfg.llm_fallback {
        let event_id = event.id().to_owned();
        ctx.state
            .record_access_log(
                AccessLogOwner::Server(ctx.server_id.as_u32()),
                "TACACS",
                Some(id.as_u32()),
                &event_id,
                event.data,
                vec![],
            )
            .await;
        return Ok(None);
    }
    let mut peek = [0; 1];
    let result = tokio::select! {
        value = tokio::time::timeout(
            cfg.handler_timeout,
            crate::llm::action_helper::call_llm(&ctx.llm_client, &ctx.state, ctx.server_id,
                Some(id), &event, &actions::TacacsProtocol),
        ) => value.context("handler deadline")??,
        _ = r.peek(&mut peek) => bail!("Peer closed or sent unexpected data while handling"),
    };
    chosen_reply(result, expected)
}

/// Consume handler values iteratively, including rejected nested results. No copy or
/// serialization occurs until all JSON and result-count budgets have passed.
pub fn chosen_reply(
    result: crate::llm::actions::executor::ExecutionResult,
    expected: &str,
) -> Result<Option<Value>> {
    let mut invalid = !result.failures.is_empty() || result.raw_actions.len() > 32;
    for value in result.raw_actions {
        invalid |= !within_json_budget(&value);
        crate::utils::json_budget::drop_iteratively(value);
    }
    let mut pending = result.protocol_results;
    let mut found = None;
    let mut seen = 0usize;
    while let Some(result) = pending.pop() {
        seen = seen.saturating_add(1);
        invalid |= seen > 64;
        match result {
            ActionResult::Multiple(items) => pending.extend(items),
            ActionResult::Custom { name, data } => {
                if name == expected && found.is_none() && within_json_budget(&data) {
                    found = Some(data);
                } else {
                    invalid = true;
                    crate::utils::json_budget::drop_iteratively(data);
                }
            }
            ActionResult::NoAction => {}
            _ => invalid = true,
        }
    }
    if invalid {
        if let Some(value) = found {
            crate::utils::json_budget::drop_iteratively(value);
        }
        bail!("handler failure, reply type/count or JSON budget");
    }
    Ok(found)
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    secret: &[u8],
    cfg: &Config,
) -> Result<()> {
    let (mut r, mut w) = socket.into_split();
    let (mut h, body) = read(ctx, id, &mut r, secret, cfg).await?;
    if h.validate().is_err() || h.sequence != 1 {
        fail(ctx, id, &mut w, h, secret, "fail_closed_header").await?;
        return Ok(());
    }
    match h.kind {
        1 => {
            let start = match parse_start(&body) {
                Ok(s) => s,
                Err(_) => {
                    fail(
                        ctx,
                        id,
                        &mut w,
                        h,
                        secret,
                        "fail_closed_authentication_body",
                    )
                    .await?;
                    return Ok(());
                }
            };
            let expected_version = if start.method == AuthType::Ascii {
                0xc0
            } else {
                0xc1
            };
            if h.version != expected_version {
                fail(
                    ctx,
                    id,
                    &mut w,
                    h,
                    secret,
                    "fail_closed_authentication_version",
                )
                .await?;
                return Ok(());
            }
            if start.action != 1
                || start.service != Service::Login
                || !matches!(start.method, AuthType::Ascii | AuthType::Pap)
            {
                let denied = auth_reply_body(&AuthReply {
                    status: AuthStatus::Fail,
                    no_echo: false,
                    server_message: "Authentication flow unsupported".into(),
                    data: String::new(),
                })?;
                send(ctx, id, &mut w, h.reply()?, &denied, secret).await?;
                decision(ctx, id, "unsupported_authentication_fail");
                return Ok(());
            }
            let mut user = start.username.clone();
            let password = if start.method == AuthType::Ascii {
                for _ in 0..3 {
                    if !user.is_empty() {
                        break;
                    }
                    let (next, value) = continuation(
                        ctx,
                        id,
                        &mut r,
                        &mut w,
                        secret,
                        cfg,
                        h,
                        AuthStatus::GetUser,
                        "Username:",
                    )
                    .await?;
                    h = next;
                    if value.abort {
                        decision(ctx, id, "client_abort");
                        return Ok(());
                    }
                    username(&value.user_message)?;
                    user = value.user_message;
                }
                if user.is_empty() {
                    let denied = auth_reply_body(&AuthReply {
                        status: AuthStatus::Fail,
                        no_echo: false,
                        server_message: "Username required".into(),
                        data: String::new(),
                    })?;
                    send(ctx, id, &mut w, h.reply()?, &denied, secret).await?;
                    decision(ctx, id, "username_retry_fail");
                    return Ok(());
                }
                let (next, value) = continuation(
                    ctx,
                    id,
                    &mut r,
                    &mut w,
                    secret,
                    cfg,
                    h,
                    AuthStatus::GetPass,
                    "Password:",
                )
                .await?;
                h = next;
                if value.abort {
                    decision(ctx, id, "client_abort");
                    return Ok(());
                }
                value.user_message
            } else {
                if user.is_empty() {
                    fail(ctx, id, &mut w, h, secret, "fail_closed_pap_username").await?;
                    return Ok(());
                }
                start.password.unwrap_or_default()
            };
            let event = Event::new(
                &actions::AUTH_EVENT,
                json!({"request":{"username":user,"password":password,"method":start.method,"privilege_level":start.privilege_level,"port":start.port,"remote_address":start.remote_address,"source_addr":peer.to_string(),"session_id":h.session_id}}),
            );
            let reply =
                match handler(ctx, id, &mut r, cfg, event, "respond_tacacs_authentication").await {
                    Ok(Some(v)) => serde_json::from_value::<AuthReply>(v)?,
                    Ok(None) => AuthReply {
                        status: AuthStatus::Fail,
                        no_echo: false,
                        server_message: String::new(),
                        data: String::new(),
                    },
                    Err(_) => {
                        fail(
                            ctx,
                            id,
                            &mut w,
                            h,
                            secret,
                            "fail_closed_authentication_handler",
                        )
                        .await?;
                        return Ok(());
                    }
                };
            ensure!(
                matches!(
                    reply.status,
                    AuthStatus::Pass | AuthStatus::Fail | AuthStatus::Error
                ) && !reply.no_echo,
                "terminal authentication reply"
            );
            send(
                ctx,
                id,
                &mut w,
                h.reply()?,
                &auth_reply_body(&reply)?,
                secret,
            )
            .await?;
            decision(
                ctx,
                id,
                match reply.status {
                    AuthStatus::Pass => "authentication_pass",
                    AuthStatus::Fail => "authentication_fail",
                    _ => "authentication_error",
                },
            );
        }
        2 | 3 => {
            if h.version != 0xc0 {
                fail(ctx, id, &mut w, h, secret, "fail_closed_request_version").await?;
                return Ok(());
            }
            let (request, kind) = match parse_request(&body, h.kind == 3) {
                Ok(v) => v,
                Err(_) => {
                    fail(ctx, id, &mut w, h, secret, "fail_closed_request_body").await?;
                    return Ok(());
                }
            };
            let mut data = serde_json::to_value(&request)?;
            data["source_addr"] = json!(peer.to_string());
            data["session_id"] = json!(h.session_id);
            if let Some(kind) = kind {
                data["record_type"] = json!(kind);
            }
            let event = Event::new(
                if h.kind == 2 {
                    &actions::AUTHOR_EVENT
                } else {
                    &actions::ACCOUNT_EVENT
                },
                json!({"request":data}),
            );
            let observed = event.data.clone();
            let expected = if h.kind == 2 {
                "respond_tacacs_authorization"
            } else {
                "record_tacacs_accounting"
            };
            let value = match handler(ctx, id, &mut r, cfg, event, expected).await {
                Ok(v) => v,
                Err(_) => {
                    fail(ctx, id, &mut w, h, secret, "fail_closed_aaa_handler").await?;
                    return Ok(());
                }
            };
            let body = if h.kind == 2 {
                let reply = if let Some(v) = value {
                    serde_json::from_value::<AuthorReply>(v)?
                } else {
                    AuthorReply {
                        status: AuthorStatus::Fail,
                        arguments: vec![],
                        server_message: String::new(),
                        data: String::new(),
                    }
                };
                ensure!(reply.status != AuthorStatus::Follow, "FOLLOW excluded");
                decision(
                    ctx,
                    id,
                    match reply.status {
                        AuthorStatus::PassAdd | AuthorStatus::PassReplace => "authorization_pass",
                        AuthorStatus::Fail => "authorization_fail",
                        _ => "authorization_error",
                    },
                );
                author_reply_body(&reply)?
            } else {
                let reply = if let Some(v) = value {
                    serde_json::from_value::<AccountReply>(v)?
                } else {
                    AccountReply {
                        status: AccountStatus::Error,
                        server_message: String::new(),
                        data: String::new(),
                    }
                };
                ensure!(reply.status != AccountStatus::Follow, "FOLLOW excluded");
                if reply.status == AccountStatus::Success {
                    ctx.state.record_access_log(
                        AccessLogOwner::Server(ctx.server_id.as_u32()), "TACACS", Some(id.as_u32()),
                        "tacacs_accounting_recorded", observed,
                        vec![json!({"type":"record_tacacs_accounting","recorded_in_shared_access_log":true,"durable_storage":false})],
                    ).await;
                    decision(ctx, id, "accounting_recorded_success");
                } else {
                    decision(ctx, id, "accounting_error");
                }
                account_reply_body(&reply)?
            };
            send(ctx, id, &mut w, h.reply()?, &body, secret).await?;
        }
        _ => unreachable!(),
    }
    tokio::time::timeout(WRITE_TIMEOUT, w.shutdown())
        .await
        .context("shutdown deadline")??;
    Ok(())
}
