//! DICOM DIMSE service user.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::dicom::dataset::{self, EXPLICIT_LE, IMPLICIT_LE};
use crate::server::dicom::pdu::{
    self, AssociateRq, Pdu, C_ECHO_RQ, C_FIND_RQ, C_STORE_RQ, NO_DATASET,
};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::DicomClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

pub const DEFAULT_CALLED: &str = "ANY-SCP";
pub const DEFAULT_CALLING: &str = "NETGET";
pub const DEFAULT_STORAGE: &[&str] = &[
    "1.2.840.10008.5.1.4.1.1.2",
    "1.2.840.10008.5.1.4.1.1.4",
    "1.2.840.10008.5.1.4.1.1.7",
];
const TIMEOUT: Duration = Duration::from_secs(30);

fn ae_ok(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= 16
        && s.bytes().all(|b| (0x20..0x7f).contains(&b) && b != b'\\')
}

struct Assoc {
    r: ReadHalf<TcpStream>,
    w: WriteHalf<TcpStream>,
    /// abstract syntax → (context id, transfer syntax)
    contexts: HashMap<String, (u8, String)>,
    peer_max: u32,
    next_id: u16,
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let opt = |k: &str, d: &str| -> Result<String> {
        Ok(p.map(|p| p.get_optional_string(k))
            .transpose()?
            .flatten()
            .unwrap_or_else(|| d.to_owned()))
    };
    let called = opt("called_ae", DEFAULT_CALLED)?;
    let calling = opt("calling_ae", DEFAULT_CALLING)?;
    ensure!(
        ae_ok(&called) && ae_ok(&calling),
        "AE titles are 1 to 16 printable characters"
    );
    let storage: Vec<String> = match p
        .map(|p| p.get_optional_array("storage_classes"))
        .transpose()?
        .flatten()
    {
        None => DEFAULT_STORAGE.iter().map(|s| s.to_string()).collect(),
        Some(list) => list
            .iter()
            .map(|s| {
                s.as_str()
                    .filter(|s| s.starts_with(pdu::STORAGE_PREFIX) && s.len() <= 64)
                    .map(str::to_owned)
                    .context("storage_classes are Storage SOP Class UIDs")
            })
            .collect::<Result<_>>()?,
    };
    ensure!(storage.len() <= 64, "at most 64 storage classes");
    let stream = tokio::time::timeout(TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("DICOM connect timed out")??;
    let local = stream.local_addr()?;
    let (mut r, mut w) = tokio::io::split(stream);
    let mut abstracts = vec![
        pdu::VERIFICATION.to_owned(),
        pdu::STUDY_ROOT_FIND.to_owned(),
        pdu::PATIENT_ROOT_FIND.to_owned(),
    ];
    abstracts.extend(storage);
    let proposed: Vec<pdu::PresentationContext> = abstracts
        .iter()
        .enumerate()
        .map(|(i, a)| pdu::PresentationContext {
            id: (2 * i + 1) as u8,
            abstract_syntax: a.clone(),
            transfer_syntaxes: vec![EXPLICIT_LE.into(), IMPLICIT_LE.into()],
        })
        .collect();
    let rq = AssociateRq {
        called: called.clone(),
        calling: calling.clone(),
        contexts: proposed.clone(),
        max_pdu: pdu::MAX_PDU,
        implementation: None,
    };
    w.write_all(&pdu::encode(&Pdu::AssociateRq(rq), &called, &calling))
        .await?;
    let answer = tokio::time::timeout(TIMEOUT, pdu::read(&mut r))
        .await
        .context("no association answer")??
        .context("connection closed during association")?;
    let (results, peer_max) = match answer {
        Pdu::AssociateAc(results, peer_max) => (results, peer_max),
        Pdu::AssociateRj {
            result,
            source,
            reason,
        } => bail!("association rejected (result {result}, source {source}, reason {reason})"),
        other => bail!("unexpected answer to A-ASSOCIATE-RQ: {other:?}"),
    };
    let mut contexts = HashMap::new();
    let mut accepted = Vec::new();
    let mut refused = Vec::new();
    for c in &proposed {
        match results.iter().find(|(id, _, _)| *id == c.id) {
            Some((id, 0, ts)) if ts == EXPLICIT_LE || ts == IMPLICIT_LE => {
                contexts.insert(c.abstract_syntax.clone(), (*id, ts.clone()));
                accepted.push(json!({"abstract_syntax": c.abstract_syntax, "transfer_syntax": ts}));
            }
            _ => refused.push(json!(c.abstract_syntax)),
        }
    }
    let assoc = Assoc {
        r,
        w,
        contexts,
        peer_max,
        next_id: 1,
    };
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(
        &actions::ASSOCIATED_EVENT,
        json!({"called_ae": called, "accepted": accepted, "refused": refused}),
    ))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = DicomClientProtocol;
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
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("DICOM client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, assoc, external, internal_rx, event_tx).await;
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("DICOM client ended: {e}"));
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
    mut a: Assoc,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        match DicomClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                a.w.write_all(&pdu::encode(&Pdu::ReleaseRq, "", ""))
                    .await
                    .ok();
                let _ = tokio::time::timeout(Duration::from_secs(5), pdu::read(&mut a.r)).await;
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
        let outcome = perform(&mut a, &action).await;
        if command.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "DICOM",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![json!({"ok": outcome.is_ok()})],
                )
                .await;
        }
        match outcome {
            Ok(data) => {
                reply(command, Ok(ClientSendOutcome::Sent { bytes_sent: 0 }));
                events
                    .send(Event::new(&actions::RESPONSE_EVENT, data))
                    .await
                    .context("DICOM event consumer stopped")?;
            }
            Err(e) => {
                Log::new(Some(&ctx.status_tx)).warn(format!("DICOM request failed: {e}"));
                reply(command, Err(anyhow::anyhow!("{e}")));
                if e.to_string().starts_with("association") {
                    return Err(e);
                }
            }
        }
    }
}

fn meaning(status: u16) -> &'static str {
    match status {
        0x0000 => "success",
        0xFF00 | 0xFF01 => "pending",
        0xFE00 => "cancel",
        s if s >> 12 == 0xB || s == 0x0001 || s == 0x0107 || s == 0x0116 => "warning",
        _ => "failure",
    }
}

async fn send_message(
    a: &mut Assoc,
    abstract_syntax: &str,
    fields: Vec<(u32, Value)>,
    ds: Option<&serde_json::Map<String, Value>>,
) -> Result<(u8, String)> {
    let (ctx_id, ts) = a
        .contexts
        .get(abstract_syntax)
        .cloned()
        .with_context(|| format!("the peer did not accept {abstract_syntax}"))?;
    let mut fields = fields;
    fields.push((
        0x0000_0800,
        pdu::us(if ds.is_some() { 0x0000 } else { NO_DATASET }),
    ));
    for p in pdu::data_pdus(ctx_id, true, &pdu::command(&fields), a.peer_max) {
        a.w.write_all(&p).await?;
    }
    if let Some(d) = ds {
        for p in pdu::data_pdus(ctx_id, false, &dataset::encode(d, &ts)?, a.peer_max) {
            a.w.write_all(&p).await?;
        }
    }
    Ok((ctx_id, ts))
}

async fn next_message(a: &mut Assoc) -> Result<pdu::Message> {
    let mut assembly = pdu::Assembly::default();
    loop {
        match tokio::time::timeout(TIMEOUT, pdu::read(&mut a.r))
            .await
            .context("association: no response in time")??
        {
            Some(Pdu::Data(pdvs)) => {
                if let Some(m) = assembly.feed(pdvs)? {
                    return Ok(m);
                }
            }
            Some(Pdu::Abort { .. }) | None => bail!("association aborted by the peer"),
            Some(other) => bail!("association: unexpected PDU {other:?}"),
        }
    }
}

async fn perform(a: &mut Assoc, action: &Value) -> Result<Value> {
    let msg_id = a.next_id;
    a.next_id = a.next_id.wrapping_add(1).max(1);
    let base = |sop: &str, field: u16| {
        vec![
            (0x0000_0002u32, pdu::ui(sop)),
            (0x0000_0100, pdu::us(field)),
            (0x0000_0110, pdu::us(msg_id)),
        ]
    };
    let (op, abstract_syntax) = match action["type"].as_str().unwrap_or_default() {
        "dicom_echo" => {
            send_message(
                a,
                pdu::VERIFICATION,
                base(pdu::VERIFICATION, C_ECHO_RQ),
                None,
            )
            .await?;
            ("echo", pdu::VERIFICATION.to_owned())
        }
        "dicom_store" => {
            let sop = action["sop_class_uid"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let instance = action["sop_instance_uid"].as_str().unwrap_or_default();
            let mut ds = action["dataset"].as_object().cloned().unwrap_or_default();
            ds.insert("00080016".into(), pdu::ui(&sop));
            ds.insert("00080018".into(), pdu::ui(instance));
            let mut f = base(&sop, C_STORE_RQ);
            f.push((0x0000_0700, pdu::us(0)));
            f.push((0x0000_1000, pdu::ui(instance)));
            send_message(a, &sop, f, Some(&ds)).await?;
            ("store", sop)
        }
        _ => {
            let model = if action["model"] == "patient_root" {
                pdu::PATIENT_ROOT_FIND
            } else {
                pdu::STUDY_ROOT_FIND
            };
            let mut ds = action["identifier"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            ds.insert(
                "00080052".into(),
                json!({"vr": "CS", "Value": [action["level"]]}),
            );
            let mut f = base(model, C_FIND_RQ);
            f.push((0x0000_0700, pdu::us(0)));
            send_message(a, model, f, Some(&ds)).await?;
            ("find", model.to_owned())
        }
    };
    let ts = a
        .contexts
        .get(&abstract_syntax)
        .map(|c| c.1.clone())
        .unwrap_or_else(|| IMPLICIT_LE.into());
    let mut matches = Vec::new();
    loop {
        let m = next_message(a).await?;
        let status = pdu::field_u16(&m.command, 0x0000_0900).unwrap_or(0xC000);
        if meaning(status) == "pending" {
            if let Some(b) = m.dataset {
                matches.push(Value::Object(dataset::decode(&b, &ts)?));
            }
            continue;
        }
        let mut out = json!({"operation": op, "status": format!("{status:04X}"), "meaning": meaning(status), "comment": pdu::field_str(&m.command, 0x0000_0902)});
        if op == "find" {
            out["matches"] = Value::Array(matches);
        }
        return Ok(out);
    }
}
