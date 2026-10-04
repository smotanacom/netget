//! DICOM DIMSE service provider. Rust owns the upper layer (association negotiation, PDU and
//! PDV framing, release, abort), the DIMSE command sets, the dataset codec, C-ECHO and C-FIND
//! matching; the handler decides associations, stored instances and query records.
pub mod actions;
pub mod dataset;
pub mod pdu;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{bail, Result};
use dataset::{EXPLICIT_LE, IMPLICIT_LE};
use pdu::{Pdu, C_CANCEL_RQ, C_ECHO_RQ, C_FIND_RQ, C_STORE_RQ, NO_DATASET};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::AsyncWriteExt;

pub const DEFAULT_AE_TITLE: &str = "NETGET";
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// The ARTIM timer: how long a new connection may take to send A-ASSOCIATE-RQ.
const ARTIM: Duration = Duration::from_secs(30);

struct Shared {
    ctx: SpawnContext,
    ae_title: String,
    idle: Duration,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let ae_title = p
        .map(|p| p.get_optional_string("ae_title"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| DEFAULT_AE_TITLE.to_owned());
    anyhow::ensure!(
        !ae_title.trim().is_empty()
            && ae_title.len() <= 16
            && ae_title
                .bytes()
                .all(|b| (0x20..0x7f).contains(&b) && b != b'\\'),
        "ae_title is 1 to 16 printable characters"
    );
    let idle = p
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=3600).contains(&idle),
        "idle_timeout_secs must be 1..=3600"
    );
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("DICOM SCP {ae_title} on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        ae_title: ae_title.trim().to_owned(),
        idle: Duration::from_secs(idle),
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            // A-ASSOCIATE-RJ (rejected-transient, local limit exceeded) for a refused connection.
            let refusal = pdu::encode(
                &Pdu::AssociateRj {
                    result: 2,
                    source: 3,
                    reason: 2,
                },
                "",
                "",
            );
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                &refusal,
                "DICOM",
                Some(&shared.ctx.status_tx),
            )
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
            shared
                .ctx
                .state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = association(&child, id, stream).await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("DICOM connection {id}: {e}"));
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

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("DICOM connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

fn service(abstract_syntax: &str) -> Option<&'static str> {
    match abstract_syntax {
        pdu::VERIFICATION => Some("verification"),
        pdu::PATIENT_ROOT_FIND => Some("patient_root_find"),
        pdu::STUDY_ROOT_FIND => Some("study_root_find"),
        s if s.starts_with(pdu::STORAGE_PREFIX) => Some("storage"),
        _ => None,
    }
}

async fn ask(
    shared: &Shared,
    id: ConnectionId,
    event: Event,
    operation: &str,
) -> Result<Value, &'static str> {
    let ctx = &shared.ctx;
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event,
        &actions::DicomProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(_) => {
            outcome(ctx, id, operation, "fail_closed_llm_error");
            return Err("fail_closed_llm_error");
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, operation, "fail_closed_invalid_reply");
        return Err("fail_closed_invalid_reply");
    }
    let mut answers = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { data, .. } => answers.push(data),
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match answers.len() {
        1 => Ok(answers.remove(0)),
        0 => {
            outcome(ctx, id, operation, "model_silent");
            Err("model_silent")
        }
        _ => {
            outcome(ctx, id, operation, "fail_closed_invalid_reply");
            Err("fail_closed_invalid_reply")
        }
    }
}

async fn send<W: tokio::io::AsyncWrite + Unpin>(
    shared: &Shared,
    id: ConnectionId,
    w: &mut W,
    bytes: &[u8],
) -> Result<()> {
    w.write_all(bytes).await?;
    shared
        .ctx
        .state
        .update_connection_stats(
            shared.ctx.server_id,
            id,
            None,
            Some(bytes.len() as u64),
            None,
            Some(1),
        )
        .await;
    Ok(())
}

async fn association(
    shared: &Shared,
    id: ConnectionId,
    stream: tokio::net::TcpStream,
) -> Result<()> {
    let ctx = &shared.ctx;
    let (mut r, mut w) = tokio::io::split(stream);
    let first = match tokio::time::timeout(ARTIM, pdu::read(&mut r)).await {
        Ok(Ok(Some(p))) => p,
        Ok(Err(e)) => {
            send(
                shared,
                id,
                &mut w,
                &pdu::encode(
                    &Pdu::Abort {
                        source: 2,
                        reason: 2,
                    },
                    "",
                    "",
                ),
            )
            .await
            .ok();
            return Err(e);
        }
        _ => return Ok(()),
    };
    let Pdu::AssociateRq(rq) = first else {
        send(
            shared,
            id,
            &mut w,
            &pdu::encode(
                &Pdu::Abort {
                    source: 2,
                    reason: 2,
                },
                "",
                "",
            ),
        )
        .await?;
        bail!("expected A-ASSOCIATE-RQ");
    };
    let reject = |reason: u8| {
        pdu::encode(
            &Pdu::AssociateRj {
                result: 1,
                source: 1,
                reason,
            },
            "",
            "",
        )
    };
    if rq.called != shared.ae_title {
        outcome(ctx, id, "associate", "protocol_refusal");
        send(shared, id, &mut w, &reject(7)).await?;
        return Ok(());
    }
    let contexts: Vec<Value> = rq.contexts.iter().map(|c| json!({"id": c.id, "abstract_syntax": c.abstract_syntax, "service": service(&c.abstract_syntax), "transfer_syntaxes": c.transfer_syntaxes})).collect();
    let event = Event::new(
        &actions::ASSOCIATE_EVENT,
        json!({"calling_ae": rq.calling, "called_ae": rq.called, "contexts": contexts, "implementation": rq.implementation}),
    );
    match ask(shared, id, event, "associate").await {
        Ok(a) if a["type"] == "dicom_accept" => outcome(ctx, id, "associate", "model_answer"),
        Ok(a) if a["type"] == "dicom_reject" => {
            outcome(ctx, id, "associate", "model_reject");
            let reason = match a["reason"].as_str() {
                Some("called_ae_not_recognized") => 7,
                Some("no_reason") => 1,
                _ => 3,
            };
            send(shared, id, &mut w, &reject(reason)).await?;
            return Ok(());
        }
        _ => {
            // Rejected-transient: the service is temporarily unable to decide.
            send(
                shared,
                id,
                &mut w,
                &pdu::encode(
                    &Pdu::AssociateRj {
                        result: 2,
                        source: 2,
                        reason: 1,
                    },
                    "",
                    "",
                ),
            )
            .await?;
            return Ok(());
        }
    }
    let mut negotiated: HashMap<u8, (String, String)> = HashMap::new();
    let results: Vec<(u8, u8, String)> = rq
        .contexts
        .iter()
        .map(|c| {
            if service(&c.abstract_syntax).is_none() {
                return (c.id, 3, IMPLICIT_LE.to_owned());
            }
            let ts = [EXPLICIT_LE, IMPLICIT_LE]
                .into_iter()
                .find(|t| c.transfer_syntaxes.iter().any(|x| x == t));
            match ts {
                Some(t) => {
                    negotiated.insert(c.id, (c.abstract_syntax.clone(), t.to_owned()));
                    (c.id, 0, t.to_owned())
                }
                None => (c.id, 4, IMPLICIT_LE.to_owned()),
            }
        })
        .collect();
    send(
        shared,
        id,
        &mut w,
        &pdu::encode(
            &Pdu::AssociateAc(results, pdu::MAX_PDU),
            &rq.called,
            &rq.calling,
        ),
    )
    .await?;
    let peer_max = rq.max_pdu;
    let mut assembly = pdu::Assembly::default();
    loop {
        let next = match tokio::time::timeout(shared.idle, pdu::read(&mut r)).await {
            Err(_) => {
                send(
                    shared,
                    id,
                    &mut w,
                    &pdu::encode(
                        &Pdu::Abort {
                            source: 2,
                            reason: 0,
                        },
                        "",
                        "",
                    ),
                )
                .await
                .ok();
                return Ok(());
            }
            Ok(Ok(Some(p))) => p,
            Ok(Ok(None)) => return Ok(()),
            Ok(Err(e)) => {
                send(
                    shared,
                    id,
                    &mut w,
                    &pdu::encode(
                        &Pdu::Abort {
                            source: 2,
                            reason: 2,
                        },
                        "",
                        "",
                    ),
                )
                .await
                .ok();
                return Err(e);
            }
        };
        ctx.state
            .update_connection_stats(ctx.server_id, id, Some(1), None, Some(1), None)
            .await;
        match next {
            Pdu::Data(pdvs) => {
                if pdvs.iter().any(|(c, _, _)| !negotiated.contains_key(c)) {
                    send(
                        shared,
                        id,
                        &mut w,
                        &pdu::encode(
                            &Pdu::Abort {
                                source: 2,
                                reason: 6,
                            },
                            "",
                            "",
                        ),
                    )
                    .await?;
                    bail!("P-DATA on a context that was not accepted");
                }
                let message = match assembly.feed(pdvs) {
                    Ok(Some(m)) => m,
                    Ok(None) => continue,
                    Err(e) => {
                        send(
                            shared,
                            id,
                            &mut w,
                            &pdu::encode(
                                &Pdu::Abort {
                                    source: 2,
                                    reason: 6,
                                },
                                "",
                                "",
                            ),
                        )
                        .await?;
                        return Err(e);
                    }
                };
                let (abs, ts) = negotiated[&message.context].clone();
                for out in dimse(shared, id, &rq.calling, &abs, &ts, message, peer_max).await {
                    send(shared, id, &mut w, &out).await?;
                }
            }
            Pdu::ReleaseRq => {
                send(shared, id, &mut w, &pdu::encode(&Pdu::ReleaseRp, "", "")).await?;
                let _ = w.shutdown().await;
                return Ok(());
            }
            Pdu::Abort { .. } => return Ok(()),
            _ => {
                send(
                    shared,
                    id,
                    &mut w,
                    &pdu::encode(
                        &Pdu::Abort {
                            source: 2,
                            reason: 2,
                        },
                        "",
                        "",
                    ),
                )
                .await?;
                bail!("unexpected PDU during the association");
            }
        }
    }
}

fn response(
    request_field: u16,
    message_id: u16,
    sop_class: &str,
    status: u16,
    instance: Option<&str>,
    has_dataset: bool,
    comment: Option<&str>,
) -> Vec<u8> {
    let mut f = vec![
        (0x0000_0002u32, pdu::ui(sop_class)),
        (0x0000_0100, pdu::us(request_field | 0x8000)),
        (0x0000_0120, pdu::us(message_id)),
        (
            0x0000_0800,
            pdu::us(if has_dataset { 0x0000 } else { NO_DATASET }),
        ),
        (0x0000_0900, pdu::us(status)),
    ];
    if let Some(i) = instance {
        f.push((0x0000_1000, pdu::ui(i)));
    }
    if let Some(c) = comment {
        f.push((0x0000_0902, json!({"vr": "LO", "Value": [c]})));
    }
    pdu::command(&f)
}

/// Answer one DIMSE message: the PDUs to send back.
async fn dimse(
    shared: &Shared,
    id: ConnectionId,
    calling: &str,
    abstract_syntax: &str,
    ts: &str,
    m: pdu::Message,
    peer_max: u32,
) -> Vec<Vec<u8>> {
    let ctx = &shared.ctx;
    let field = pdu::field_u16(&m.command, 0x0000_0100).unwrap_or(0);
    let msg_id = pdu::field_u16(&m.command, 0x0000_0110).unwrap_or(0);
    let sop_class =
        pdu::field_str(&m.command, 0x0000_0002).unwrap_or_else(|| abstract_syntax.to_owned());
    let reply = |cmd: Vec<u8>| pdu::data_pdus(m.context, true, &cmd, peer_max);
    match field {
        C_ECHO_RQ => {
            outcome(ctx, id, "c-echo", "protocol_answer");
            reply(response(
                C_ECHO_RQ, msg_id, &sop_class, 0x0000, None, false, None,
            ))
        }
        C_STORE_RQ => {
            let instance = pdu::field_str(&m.command, 0x0000_1000).unwrap_or_default();
            let bytes = m.dataset.unwrap_or_default();
            let decoded = dataset::decode(&bytes, ts);
            let ds = match decoded {
                Ok(d) => d,
                Err(_) => {
                    return reply(response(
                        C_STORE_RQ,
                        msg_id,
                        &sop_class,
                        0xC000,
                        Some(&instance),
                        false,
                        Some("dataset could not be decoded"),
                    ))
                }
            };
            if dataset::text(&ds, 0x0008_0018).is_some_and(|i| i != instance)
                || dataset::text(&ds, 0x0008_0016).is_some_and(|c| c != sop_class)
            {
                outcome(ctx, id, "c-store", "protocol_refusal");
                return reply(response(
                    C_STORE_RQ,
                    msg_id,
                    &sop_class,
                    0xA900,
                    Some(&instance),
                    false,
                    Some("SOP UIDs differ from the command"),
                ));
            }
            let event = Event::new(
                &actions::STORE_EVENT,
                json!({"calling_ae": calling, "sop_class_uid": sop_class, "sop_instance_uid": instance, "dataset": ds, "bytes": bytes.len()}),
            );
            let (status, comment) = match ask(shared, id, event, "c-store").await {
                Ok(a) if a["type"] == "dicom_store_status" => {
                    let name = a["status"].as_str().unwrap_or_default();
                    let code = actions::STORE_STATUS
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, c)| *c)
                        .unwrap_or(0xC000);
                    outcome(
                        ctx,
                        id,
                        "c-store",
                        if code == 0 || code >> 12 == 0xB {
                            "model_answer"
                        } else {
                            "model_reject"
                        },
                    );
                    (code, a["comment"].as_str().map(str::to_owned))
                }
                Ok(_) => {
                    outcome(ctx, id, "c-store", "fail_closed_invalid_reply");
                    (0xA700, None)
                }
                Err(_) => (
                    0xA700,
                    Some("the service cannot store right now".to_owned()),
                ),
            };
            reply(response(
                C_STORE_RQ,
                msg_id,
                &sop_class,
                status,
                Some(&instance),
                false,
                comment.as_deref(),
            ))
        }
        C_FIND_RQ => {
            let model = if abstract_syntax == pdu::PATIENT_ROOT_FIND {
                "patient_root"
            } else {
                "study_root"
            };
            let identifier = match m.dataset.as_deref().map(|b| dataset::decode(b, ts)) {
                Some(Ok(d)) => d,
                _ => {
                    return reply(response(
                        C_FIND_RQ,
                        msg_id,
                        &sop_class,
                        0xC000,
                        None,
                        false,
                        Some("identifier could not be decoded"),
                    ))
                }
            };
            let Some(level) = dataset::text(&identifier, 0x0008_0052)
                .filter(|l| matches!(l.as_str(), "PATIENT" | "STUDY" | "SERIES" | "IMAGE"))
            else {
                outcome(ctx, id, "c-find", "protocol_refusal");
                return reply(response(
                    C_FIND_RQ,
                    msg_id,
                    &sop_class,
                    0xA900,
                    None,
                    false,
                    Some("QueryRetrieveLevel is missing or invalid"),
                ));
            };
            let event = Event::new(
                &actions::FIND_EVENT,
                json!({"calling_ae": calling, "model": model, "level": level, "identifier": identifier}),
            );
            let matches = match ask(shared, id, event, "c-find").await {
                Ok(a) if a["type"] == "dicom_find_matches" => {
                    a["matches"].as_array().cloned().unwrap_or_default()
                }
                Ok(a) if a["type"] == "dicom_find_failed" => {
                    outcome(ctx, id, "c-find", "model_reject");
                    let code = if a["status"] == "out_of_resources" {
                        0xA700
                    } else {
                        0xC000
                    };
                    return reply(response(
                        C_FIND_RQ, msg_id, &sop_class, code, None, false, None,
                    ));
                }
                _ => {
                    return reply(response(
                        C_FIND_RQ,
                        msg_id,
                        &sop_class,
                        0xC000,
                        None,
                        false,
                        Some("the service cannot query right now"),
                    ))
                }
            };
            let mut out = Vec::new();
            for rec in matches.iter().filter_map(Value::as_object) {
                if !dataset::matches(&identifier, rec) {
                    continue;
                }
                let projected: Map<String, Value> = dataset::project(&identifier, rec);
                let Ok(encoded) = dataset::encode(&projected, ts) else {
                    outcome(ctx, id, "c-find", "fail_closed_invalid_reply");
                    return reply(response(
                        C_FIND_RQ,
                        msg_id,
                        &sop_class,
                        0xC000,
                        None,
                        false,
                        Some("a record could not be encoded"),
                    ));
                };
                out.extend(reply(response(
                    C_FIND_RQ, msg_id, &sop_class, 0xFF00, None, true, None,
                )));
                out.extend(pdu::data_pdus(m.context, false, &encoded, peer_max));
            }
            outcome(ctx, id, "c-find", "model_answer");
            out.extend(reply(response(
                C_FIND_RQ, msg_id, &sop_class, 0x0000, None, false, None,
            )));
            out
        }
        C_CANCEL_RQ => vec![],
        other => {
            outcome(ctx, id, "dimse", "protocol_refusal");
            reply(response(
                other, msg_id, &sop_class, 0x0211, None, false, None,
            ))
        }
    }
}
