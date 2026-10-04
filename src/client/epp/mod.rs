//! EPP registrar client: TLS, the greeting, an optional login, then the handler's commands.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::Client;
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::epp::{wire, xml};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::EppClientProtocol;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(30);
/// handler → command → response → handler … stops here (a provisioning flow is about ten deep);
/// an injected command starts afresh.
const MAX_FOLLOWUP_DEPTH: usize = 16;

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

struct Conn {
    reader: ReadHalf<Box<dyn Stream>>,
    writer: WriteHalf<Box<dyn Stream>>,
    serial: u64,
}

impl Conn {
    async fn read(&mut self) -> Result<Value> {
        let frame = tokio::time::timeout(TIMEOUT, wire::read_frame(&mut self.reader))
            .await
            .context("the server did not answer in time")??
            .context("the server closed the session")?;
        wire::read_response(&xml::parse(&frame)?)
    }

    /// Send one rendered command and read its answer.
    async fn exchange(&mut self, command: &str, element: &str) -> Result<Value> {
        self.serial += 1;
        let cl_trid = format!("NG-C-{:06}", self.serial);
        wire::write_frame(
            &mut self.writer,
            &actions::document(command, element, &cl_trid),
        )
        .await?;
        self.read().await
    }
}

async fn open(ctx: &ConnectContext) -> Result<(Box<dyn Stream>, SocketAddr)> {
    let p = ctx.startup_params.as_ref();
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let tls = p
        .map(|p| p.get_optional_bool("tls"))
        .transpose()?
        .flatten()
        .unwrap_or(true);
    let tcp = tokio::time::timeout(TIMEOUT, tokio::net::TcpStream::connect(&ctx.remote_addr))
        .await
        .context("connecting timed out")??;
    let local = tcp.local_addr()?;
    if !tls {
        return Ok((Box::new(tcp), local));
    }
    let mut roots = rustls::RootCertStore::empty();
    match get("ca_cert_path")? {
        Some(ca) => {
            let pem = tokio::fs::read(&ca)
                .await
                .with_context(|| format!("reading {ca}"))?;
            for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
                roots.add(cert.with_context(|| format!("{ca} holds no PEM certificate"))?)?;
            }
            ensure!(!roots.is_empty(), "{ca} holds no PEM certificate");
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let host = match get("server_name")? {
        Some(n) => n,
        None => ctx
            .remote_addr
            .rsplit_once(':')
            .map(|(h, _)| h.trim_matches(['[', ']']).to_owned())
            .context("remote_addr is host:port")?,
    };
    let name = rustls::pki_types::ServerName::try_from(host.clone())
        .with_context(|| format!("{host} is not a TLS name"))?;
    let stream = tokio::time::timeout(
        TIMEOUT,
        tokio_rustls::TlsConnector::from(Arc::new(config)).connect(name, tcp),
    )
    .await
    .context("the TLS handshake timed out")??;
    Ok((Box::new(stream), local))
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let get = |name| {
        p.map(|p| p.get_optional_string(name))
            .transpose()
            .map(Option::flatten)
    };
    let credentials = match (get("client_id")?, get("password")?) {
        (Some(c), Some(pw)) => Some((c, pw)),
        (None, None) => None,
        _ => bail!("client_id and password go together"),
    };
    let (stream, local) = open(&ctx).await?;
    let (reader, writer) = tokio::io::split(stream);
    let mut conn = Conn {
        reader,
        writer,
        serial: 0,
    };
    let greeting = conn.read().await?;
    ensure!(greeting["greeting"] == true, "the server did not greet");
    let mut info = json!({"sv_id": greeting["sv_id"], "obj_uris": greeting["obj_uris"], "ext_uris": greeting["ext_uris"], "login": null});
    if let Some((client_id, password)) = &credentials {
        let r = conn
            .exchange("login", &actions::login(client_id, password))
            .await?;
        let code = r["code"].as_u64().unwrap_or(0);
        ensure!(
            code == 1000,
            "login refused with {code}: {}",
            r["reason"]
                .as_str()
                .or(r["message"].as_str())
                .unwrap_or_default()
        );
        info["login"] = json!({"code": code, "message": r["message"], "client_id": client_id});
    }
    Log::new(Some(&ctx.status_tx)).info(format!(
        "EPP session with {} ({})",
        ctx.remote_addr, greeting["sv_id"]
    ));
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, info))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = EppClientProtocol;
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
                    if let Some(m) = result.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, m)
                            .await;
                    }
                    for action in result.actions {
                        if internal_tx.send(action).await.is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx)).warn(format!("EPP client handler: {e}"))
                }
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &mut conn, external, internal_rx, &event_tx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("EPP session ended: {e:#}"));
        }
        session_ctx
            .state
            .update_client_status(session_ctx.client_id, ClientStatus::Disconnected)
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

async fn run(
    ctx: &ConnectContext,
    conn: &mut Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: &mpsc::Sender<Event>,
) -> Result<()> {
    let mut depth = 0usize;
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let rendered = EppClientProtocol
            .execute_action(action.clone())
            .and_then(|_| actions::render(&action));
        let (outcome, done) = match rendered {
            Err(e) => (
                ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
                false,
            ),
            Ok((verb, object, element)) => {
                let answer = conn.exchange(&verb, &element).await;
                let answer = match answer {
                    Ok(a) => a,
                    Err(e) => {
                        if let Some(c) = command {
                            crate::client::command_support::reply(
                                c,
                                Ok(ClientSendOutcome::Rejected {
                                    error: format!("{e:#}"),
                                }),
                            );
                        }
                        return Err(e);
                    }
                };
                let mut response = answer;
                if response["greeting"] == true {
                    response = json!({"code": 1000, "message": "greeting", "data": response});
                }
                response["command"] = json!(verb);
                response["object"] = json!(object);
                depth = if command.is_some() { 0 } else { depth + 1 };
                if depth <= MAX_FOLLOWUP_DEPTH {
                    events
                        .send(Event::new(&actions::RESPONSE_EVENT, response))
                        .await
                        .ok();
                } else {
                    Log::new(Some(&ctx.status_tx)).warn(format!("EPP client: follow-up depth {MAX_FOLLOWUP_DEPTH} reached; the response is not raised"));
                }
                (
                    ClientSendOutcome::Sent {
                        bytes_sent: element.len(),
                    },
                    verb == "logout",
                )
            }
        };
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "EPP",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![serde_json::to_value(&outcome).unwrap_or(Value::Null)],
                )
                .await;
            crate::client::command_support::reply(c, Ok(outcome));
        }
        if done {
            return Ok(());
        }
    }
}
