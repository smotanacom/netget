//! EPP registry server (RFC 5730, RFC 5734). Rust owns TLS, framing, the greeting, hello,
//! login/logout and session state; the handler answers every domain, host and contact command.
pub mod actions;
pub mod wire;
pub mod xml;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite};
use wire::{Command, Refusal};

pub const DEFAULT_SERVER_ID: &str = "NetGet EPP";
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
const TLS_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FAILED_LOGINS: u32 = 3;
const OBJECTS: &[&str] = &["domain", "host", "contact"];

struct Shared {
    ctx: SpawnContext,
    server_id: String,
    clients: BTreeMap<String, String>,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let server_id = get("server_id")?.unwrap_or_else(|| DEFAULT_SERVER_ID.into());
    anyhow::ensure!(
        (3..=64).contains(&server_id.len()),
        "server_id is 3 to 64 characters"
    );
    let mut clients = BTreeMap::new();
    if let Some(m) = p
        .map(|p| p.get_optional_object("clients"))
        .transpose()?
        .flatten()
    {
        anyhow::ensure!(m.len() <= 64, "at most 64 clients");
        for (k, v) in m {
            let v = v
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("client passwords are strings"))?;
            clients.insert(k.clone(), v.to_owned());
        }
    }
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!((1..=3600).contains(&idle), "idle_timeout_secs is 1 to 3600");
    let tls_on = p
        .map(|p| p.get_optional_bool("tls"))
        .transpose()?
        .flatten()
        .unwrap_or(true);
    let tls = match (tls_on, get("tls_cert_file")?, get("tls_key_file")?) {
        (false, None, None) => None,
        (false, _, _) => anyhow::bail!("tls is false but a certificate was given"),
        (true, Some(c), Some(k)) => Some(tokio_rustls::TlsAcceptor::from(
            crate::server::tls_cert_manager::load_tls_config_from_files(&c, &k)?,
        )),
        (true, None, None) => {
            let spec = crate::server::tls_cert_manager::CertificateSpec {
                common_name: "localhost".into(),
                san_dns_names: vec!["localhost".into()],
                validity_days: 30,
                organization: Some("NetGet".into()),
                organizational_unit: Some("EPP".into()),
            };
            let (cert, key) = crate::server::tls_cert_manager::generate_self_signed_cert(&spec)?;
            let pem = cert.pem();
            ctx.state
                .with_server_mut(ctx.server_id, |s| {
                    s.set_protocol_field("certificate_pem".into(), json!(pem))
                })
                .await;
            Some(tokio_rustls::TlsAcceptor::from(
                crate::server::tls_cert_manager::create_rustls_server_config(&cert, &key)?,
            ))
        }
        _ => anyhow::bail!("tls_cert_file and tls_key_file go together"),
    };
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "EPP registry on {local} ({})",
        if tls.is_some() { "TLS" } else { "plain TCP" }
    ));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        server_id,
        clients,
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) =
                match accept_bounded(&listener, &limiter, b"", "EPP", Some(&shared.ctx.status_tx))
                    .await
                {
                    Ok(v) => v,
                    Err(_) => break,
                };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            shared
                .ctx
                .state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
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
            let child = shared.clone();
            let tls = tls.clone();
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    let served = match tls {
                        Some(acceptor) => {
                            match tokio::time::timeout(TLS_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(s)) => session(&child, id, s).await,
                                Ok(Err(e)) => Err(anyhow::anyhow!("TLS handshake: {e}")),
                                Err(_) => Err(anyhow::anyhow!("TLS handshake timed out")),
                            }
                        }
                        None => session(&child, id, stream).await,
                    };
                    if let Err(e) = served {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("EPP connection {id}: {e:#}"));
                    }
                    child
                        .ctx
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.ctx.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    ctx.state.register_server_task(server_id, accept).await;
    Ok(local)
}

fn log_decision(ctx: &SpawnContext, id: ConnectionId, what: &str, decision: &str, code: u16) {
    let line = format!("EPP connection {id} {what} decision={decision} code={code}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(line);
    } else {
        log.info(line);
    }
}

struct Answer {
    code: u16,
    reason: Option<String>,
    res_data: String,
}

impl Answer {
    fn of(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: Some(reason.into()).filter(|r: &String| !r.is_empty()),
            res_data: String::new(),
        }
    }
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    shared: &Shared,
    id: ConnectionId,
    stream: S,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let now = || {
        chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    };
    let greeting = wire::greeting(&shared.server_id, &now(), OBJECTS);
    wire::write_frame(&mut writer, &greeting).await?;
    let mut client: Option<String> = None;
    let mut failed_logins = 0u32;
    let mut serial = 0u64;
    loop {
        let frame = match tokio::time::timeout(shared.idle, wire::read_frame(&mut reader)).await {
            Err(_) => {
                log_decision(ctx, id, "idle", "protocol_refusal", 2500);
                return Ok(());
            }
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => {
                log_decision(ctx, id, "frame", "protocol_refusal", 2500);
                serial += 1;
                let sv = format!("NG-{id}-{serial}");
                let _ = wire::write_frame(
                    &mut writer,
                    &wire::response(
                        2500,
                        wire::message(2500),
                        Some(&e.to_string()),
                        "",
                        None,
                        &sv,
                    ),
                )
                .await;
                return Ok(());
            }
        };
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                Some(frame.len() as u64 + 4),
                None,
                Some(1),
                None,
            )
            .await;
        serial += 1;
        let sv_trid = format!("NG-{id}-{serial}");
        let parsed = xml::parse(&frame)
            .map_err(|e| Refusal {
                code: 2001,
                reason: e.to_string(),
            })
            .and_then(|root| wire::command(&root));
        let (answer, cl_trid, close) = match parsed {
            Err(r) => {
                log_decision(ctx, id, "command", "protocol_refusal", r.code);
                (Answer::of(r.code, r.reason), None, false)
            }
            Ok(frame) => {
                let cl = frame.cl_trid.clone();
                match frame.command {
                    Command::Hello => {
                        wire::write_frame(
                            &mut writer,
                            &wire::greeting(&shared.server_id, &now(), OBJECTS),
                        )
                        .await?;
                        continue;
                    }
                    Command::Login {
                        client_id,
                        password,
                        version,
                        obj_uris,
                    } => {
                        if client.is_some() {
                            (Answer::of(2002, "already logged in"), cl, false)
                        } else if version != "1.0" {
                            (Answer::of(2100, "only version 1.0 is served"), cl, false)
                        } else if let Some(u) = obj_uris
                            .iter()
                            .find(|u| wire::object_of(u).is_none() && u.as_str() != xml::EPP)
                        {
                            (Answer::of(2307, format!("{u} is not served")), cl, false)
                        } else if !shared.clients.is_empty()
                            && shared.clients.get(&client_id) != Some(&password)
                        {
                            failed_logins += 1;
                            if failed_logins >= MAX_FAILED_LOGINS {
                                log_decision(ctx, id, "login", "protocol_refusal", 2501);
                                (Answer::of(2501, "too many failed logins"), cl, true)
                            } else {
                                log_decision(ctx, id, "login", "protocol_refusal", 2200);
                                (Answer::of(2200, ""), cl, false)
                            }
                        } else {
                            Log::new(Some(&ctx.status_tx))
                                .info(format!("EPP connection {id} logged in as {client_id}"));
                            client = Some(client_id);
                            (Answer::of(1000, ""), cl, false)
                        }
                    }
                    Command::Logout => (Answer::of(1500, ""), cl, true),
                    _ if client.is_none() => (Answer::of(2002, "log in first"), cl, false),
                    Command::Poll { op, .. } if op == "req" => (Answer::of(1300, ""), cl, false),
                    Command::Poll { .. } => (Answer::of(2303, "no such message"), cl, false),
                    Command::Object(command, object, fields) => {
                        let answer = ask(
                            shared,
                            id,
                            &command,
                            &object,
                            fields,
                            client.as_deref().unwrap_or_default(),
                            cl.as_deref(),
                        )
                        .await;
                        (answer, cl, false)
                    }
                }
            }
        };
        let xml = wire::response(
            answer.code,
            wire::message(answer.code),
            answer.reason.as_deref(),
            &answer.res_data,
            cl_trid.as_deref(),
            &sv_trid,
        );
        wire::write_frame(&mut writer, &xml).await?;
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                id,
                None,
                Some(xml.len() as u64 + 4),
                None,
                Some(1),
            )
            .await;
        if close {
            return Ok(());
        }
    }
}

async fn ask(
    shared: &Shared,
    id: ConnectionId,
    command: &str,
    object: &str,
    fields: Value,
    client: &str,
    cl_trid: Option<&str>,
) -> Answer {
    let ctx = &shared.ctx;
    let what = format!("{object}:{command}");
    let event = Event::new(
        &actions::COMMAND_EVENT,
        json!({"command": command, "object": object, "fields": fields, "client_id": client, "cl_trid": cl_trid}),
    );
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::EppProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            log_decision(ctx, id, &what, "fail_closed_llm_error", 2400);
            return Answer::of(2400, "");
        }
    };
    if !result.failures.is_empty() {
        log_decision(ctx, id, &what, "fail_closed_invalid_reply", 2400);
        return Answer::of(2400, "");
    }
    let Some(action) = result.protocol_results.into_iter().find_map(|r| match r {
        ActionResult::Custom { data, .. } => Some(data),
        _ => None,
    }) else {
        log_decision(ctx, id, &what, "model_silent", 2400);
        return Answer::of(2400, "");
    };
    match wire::res_data(&action, command, object) {
        Ok(res_data) => {
            let default = if action["type"] == "epp_result" {
                2400
            } else {
                1000
            };
            let code = action["code"].as_u64().map(|c| c as u16).unwrap_or(default);
            log_decision(
                ctx,
                id,
                &what,
                if code >= 2000 {
                    "model_reject"
                } else {
                    "model_answer"
                },
                code,
            );
            Answer {
                code,
                reason: action["reason"]
                    .as_str()
                    .map(str::to_owned)
                    .filter(|r| !r.is_empty()),
                res_data,
            }
        }
        Err(e) => {
            log_decision(ctx, id, &what, "fail_closed_invalid_reply", 2400);
            Log::new(Some(&ctx.status_tx)).warn(format!("EPP answer for {what} unusable: {e}"));
            Answer::of(2400, "")
        }
    }
}
