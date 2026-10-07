//! LwM2M 1.1 device over the server's CoAP exchange and content code: registers its object
//! instances, keeps the registration fresh, and answers the server's requests through the
//! handler.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::coap::codec::{self, CoapMessage};
use crate::server::lwm2m::content;
use crate::server::lwm2m::exchange::{path_options, uint_option, Exchange, OPT_OBSERVE};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::Lwm2mClientProtocol;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value as Json};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_LIFETIME: u64 = 300;
pub const DEFAULT_OBJECTS: &[&str] = &["/1/0", "/3/0"];

struct Device {
    ex: Arc<Exchange>,
    server: SocketAddr,
    endpoint: String,
    lifetime: u64,
    objects: Vec<String>,
    location: Mutex<String>,
    observations: Mutex<HashMap<String, (Vec<u8>, u32)>>,
}

struct Asked {
    event: Event,
    answer: Option<oneshot::Sender<Vec<Json>>>,
}

impl Device {
    async fn register(&self) -> Result<String> {
        let mut options = vec![
            (codec::OPT_URI_PATH, b"rd".to_vec()),
            uint_option(codec::OPT_CONTENT_FORMAT, content::CF_LINK as u32),
        ];
        for q in [
            format!("ep={}", self.endpoint),
            format!("lt={}", self.lifetime),
            "lwm2m=1.1".into(),
            "b=U".into(),
        ] {
            options.push((codec::OPT_URI_QUERY, q.into_bytes()));
        }
        let mut links = String::from("</>;rt=\"oma.lwm2m\";ct=110");
        for o in &self.objects {
            links.push_str(&format!(",<{o}>"));
        }
        let r = self
            .ex
            .request(
                self.server,
                codec::CODE_POST,
                options,
                links.into_bytes(),
                None,
            )
            .await?;
        ensure!(
            r.code == codec::code(2, 1),
            "the server answered the registration with {}",
            codec::code_to_string(r.code)
        );
        let location: Vec<String> = r
            .option_values(codec::OPT_LOCATION_PATH)
            .iter()
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect();
        ensure!(
            !location.is_empty(),
            "the server gave the registration no location"
        );
        let loc = format!("/{}", location.join("/"));
        *self.location.lock().unwrap_or_else(|e| e.into_inner()) = loc.clone();
        Ok(loc)
    }

    async fn update(&self) -> Result<()> {
        let loc = self
            .location
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut options = path_options(&loc);
        options.push((
            codec::OPT_URI_QUERY,
            format!("lt={}", self.lifetime).into_bytes(),
        ));
        let r = self
            .ex
            .request(self.server, codec::CODE_POST, options, vec![], None)
            .await?;
        if r.code == codec::CODE_NOT_FOUND {
            self.register().await?;
        } else {
            ensure!(
                codec::code_class(r.code) == 2,
                "the server answered the update with {}",
                codec::code_to_string(r.code)
            );
        }
        Ok(())
    }

    fn known(&self, path: &str) -> bool {
        let object = path.split('/').nth(1).unwrap_or_default();
        self.objects
            .iter()
            .any(|o| o.split('/').nth(1) == Some(object))
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let p = ctx.startup_params.as_ref();
    let endpoint = p
        .map(|p| p.get_optional_string("endpoint"))
        .transpose()?
        .flatten()
        .unwrap_or_else(|| format!("netget-{}", ctx.client_id.as_u32()));
    ensure!(
        !endpoint.is_empty() && endpoint.len() <= 256 && !endpoint.contains(['&', '=', ' ']),
        "endpoint is a name without & = or spaces"
    );
    let lifetime = p
        .map(|p| p.get_optional_u64("lifetime"))
        .transpose()?
        .flatten()
        .unwrap_or(DEFAULT_LIFETIME);
    ensure!(
        (10..=86400).contains(&lifetime),
        "lifetime is 10 to 86400 seconds"
    );
    let objects: Vec<String> = match p
        .map(|p| p.get_optional_array("objects"))
        .transpose()?
        .flatten()
    {
        Some(list) => list
            .iter()
            .map(|o| o.as_str().map(str::to_owned).context("objects are paths"))
            .collect::<Result<_>>()?,
        None => DEFAULT_OBJECTS.iter().map(|s| s.to_string()).collect(),
    };
    ensure!(
        !objects.is_empty() && objects.len() <= 256,
        "objects lists 1 to 256 instances"
    );
    for o in &objects {
        ensure!(
            content::valid_path(o) && o.split('/').count() <= 3,
            "{o:?} is an object or object instance path"
        );
    }
    let server: SocketAddr = tokio::net::lookup_host(&ctx.remote_addr)
        .await?
        .next()
        .context("the address does not resolve")?;
    let socket = Arc::new(
        UdpSocket::bind(if server.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        })
        .await?,
    );
    // Connected, so a server that is not there is reported now rather than after the CoAP
    // retransmission schedule (about a minute and a half).
    socket.connect(server).await?;
    let local = socket.local_addr()?;
    let ex = Exchange::connected(socket, server);
    let (req_tx, mut req_rx) = mpsc::channel(64);
    let runner = tokio::spawn(ex.clone().run(req_tx));
    let device = Arc::new(Device {
        ex: ex.clone(),
        server,
        endpoint,
        lifetime,
        objects,
        location: Mutex::new(String::new()),
        observations: Mutex::new(HashMap::new()),
    });
    let location = match device.register().await {
        Ok(l) => l,
        Err(e) => {
            runner.abort();
            return Err(e);
        }
    };
    ctx.state.register_client_task(ctx.client_id, runner).await;
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Json>(64);
    let (ask_tx, mut ask_rx) = mpsc::channel::<Asked>(64);
    ask_tx.try_send(Asked {
        event: Event::new(
            &actions::REGISTERED_EVENT,
            json!({"server": server.to_string(), "location": location, "lifetime": lifetime}),
        ),
        answer: None,
    })?;

    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = Lwm2mClientProtocol;
        while let Some(asked) = ask_rx.recv().await {
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
            let actions = match call_llm_for_client(
                &events_ctx.llm_client,
                &events_ctx.state,
                events_ctx.client_id.to_string(),
                &instruction,
                &memory,
                Some(&asked.event),
                &protocol,
                &events_ctx.status_tx,
            )
            .await
            {
                Ok(r) => {
                    if let Some(m) = r.memory_updates {
                        events_ctx
                            .state
                            .set_memory_for_client(events_ctx.client_id, m)
                            .await;
                    }
                    r.actions
                }
                Err(e) => {
                    Log::new(Some(&events_ctx.status_tx))
                        .warn(format!("LwM2M client handler: {e}"));
                    vec![]
                }
            };
            for a in &actions {
                if matches!(
                    a["type"].as_str(),
                    Some("lwm2m_notify" | "lwm2m_update" | "disconnect")
                ) && internal_tx.send(a.clone()).await.is_err()
                {
                    return;
                }
            }
            if let Some(reply) = asked.answer {
                let _ = reply.send(actions);
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;

    let (d, tx, status) = (device.clone(), ask_tx.clone(), ctx.status_tx.clone());
    let (state, client_id) = (ctx.state.clone(), ctx.client_id);
    let requests = tokio::spawn(async move {
        while let Some((peer, m)) = req_rx.recv().await {
            let (d, tx, status) = (d.clone(), tx.clone(), status.clone());
            state
                .spawn_client_task(client_id, async move {
                    if let Err(e) = serve(&d, &tx, peer, &m).await {
                        Log::new(Some(&status)).debug(format!("LwM2M request: {e:#}"));
                    }
                })
                .await;
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, requests)
        .await;

    let d = device.clone();
    let status = ctx.status_tx.clone();
    let refresher = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs((d.lifetime / 2).max(5))).await;
            if let Err(e) = d.update().await {
                Log::new(Some(&status)).warn(format!("LwM2M registration update: {e:#}"));
            }
        }
    });
    ctx.state
        .register_client_task(ctx.client_id, refresher)
        .await;

    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = run(&session_ctx, &device, external, internal_rx).await {
            Log::new(Some(&session_ctx.status_tx)).warn(format!("LwM2M client ended: {e:#}"));
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

async fn ask(tx: &mpsc::Sender<Asked>, event: Event) -> Vec<Json> {
    let (back, answer) = oneshot::channel();
    if tx
        .send(Asked {
            event,
            answer: Some(back),
        })
        .await
        .is_err()
    {
        return vec![];
    }
    answer.await.unwrap_or_default()
}

/// Answer one request from the server.
async fn serve(
    d: &Device,
    tx: &mpsc::Sender<Asked>,
    peer: SocketAddr,
    m: &CoapMessage,
) -> Result<()> {
    let path = m.uri_path();
    let ex = &d.ex;
    if !content::valid_path(&path) || !d.known(&path) {
        return ex
            .respond(peer, m, codec::CODE_NOT_FOUND, vec![], vec![])
            .await;
    }
    let depth = path.split('/').count() - 1;
    let format = m.option_uint(codec::OPT_CONTENT_FORMAT);
    let decision = |actions: &[Json], ok: u8| -> u8 {
        match actions.iter().find(|a| {
            matches!(
                a["type"].as_str(),
                Some("lwm2m_ok" | "lwm2m_error" | "lwm2m_content")
            )
        }) {
            Some(a) if a["type"] == "lwm2m_error" => a["code"]
                .as_str()
                .and_then(actions::error_code)
                .unwrap_or(codec::CODE_NOT_FOUND),
            Some(_) => ok,
            None => codec::CODE_SERVICE_UNAVAILABLE,
        }
    };
    match m.code {
        codec::CODE_GET => {
            let accept = m.option_uint(codec::OPT_ACCEPT);
            if accept == Some(content::CF_LINK as u32) {
                let links: Vec<String> = d
                    .objects
                    .iter()
                    .filter(|o| o.starts_with(&path) || path.starts_with(o.as_str()))
                    .cloned()
                    .collect();
                return ex
                    .respond(
                        peer,
                        m,
                        codec::CODE_CONTENT,
                        vec![uint_option(
                            codec::OPT_CONTENT_FORMAT,
                            content::CF_LINK as u32,
                        )],
                        content::links_encode(&links),
                    )
                    .await;
            }
            let observe = m.option_uint(OPT_OBSERVE);
            if observe == Some(1) {
                d.observations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&path);
            }
            let start = observe == Some(0);
            let actions = ask(
                tx,
                Event::new(
                    &actions::READ_EVENT,
                    json!({"path": path, "observe": start}),
                ),
            )
            .await;
            let code = decision(&actions, codec::CODE_CONTENT);
            let Some(values) = actions
                .iter()
                .find(|a| a["type"] == "lwm2m_content")
                .and_then(|a| a["values"].as_array())
                .filter(|_| code == codec::CODE_CONTENT)
            else {
                return ex.respond(peer, m, code, vec![], vec![]).await;
            };
            let text = accept == Some(content::CF_TEXT as u32)
                || (accept.is_none() && depth >= 3 && values.len() == 1);
            let (cf, payload) = if text {
                (content::CF_TEXT, content::text_encode(&values[0])?)
            } else {
                (content::CF_SENML_JSON, content::senml_encode(values)?)
            };
            let mut options = vec![uint_option(codec::OPT_CONTENT_FORMAT, cf as u32)];
            if start {
                options.insert(0, uint_option(OPT_OBSERVE, 0));
                d.observations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, (m.token.clone(), 0));
            }
            ex.respond(peer, m, codec::CODE_CONTENT, options, payload)
                .await
        }
        codec::CODE_PUT | codec::CODE_POST => {
            let values = content::decode(format, &path, &m.payload).unwrap_or_default();
            let (event, ok) = if m.code == codec::CODE_POST && depth == 1 {
                (
                    Event::new(
                        &actions::CREATE_EVENT,
                        json!({"path": path, "values": values}),
                    ),
                    codec::code(2, 1),
                )
            } else if m.code == codec::CODE_POST
                && depth == 3
                && format != Some(content::CF_SENML_JSON as u32)
            {
                let args = String::from_utf8_lossy(&m.payload).into_owned();
                (
                    Event::new(
                        &actions::EXECUTE_EVENT,
                        json!({"path": path, "arguments": args}),
                    ),
                    codec::code(2, 4),
                )
            } else {
                let mode = if m.code == codec::CODE_PUT {
                    "replace"
                } else {
                    "update"
                };
                (
                    Event::new(
                        &actions::WRITE_EVENT,
                        json!({"path": path, "values": values, "mode": mode}),
                    ),
                    codec::code(2, 4),
                )
            };
            let actions = ask(tx, event).await;
            ex.respond(peer, m, decision(&actions, ok), vec![], vec![])
                .await
        }
        codec::CODE_DELETE => {
            let actions = ask(
                tx,
                Event::new(&actions::DELETE_EVENT, json!({"path": path})),
            )
            .await;
            ex.respond(
                peer,
                m,
                decision(&actions, codec::code(2, 2)),
                vec![],
                vec![],
            )
            .await
        }
        _ => {
            ex.respond(peer, m, codec::CODE_METHOD_NOT_ALLOWED, vec![], vec![])
                .await
        }
    }
}

async fn run(
    ctx: &ConnectContext,
    d: &Device,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Json>,
) -> Result<()> {
    loop {
        let (action, command) = tokio::select! {
            c = external.recv() => match c { Some(c) => (c.action.clone(), Some(c)), None => return Ok(()) },
            a = internal.recv() => match a { Some(a) => (a, None), None => return Ok(()) },
        };
        let outcome = match Lwm2mClientProtocol.execute_action(action.clone()) {
            Err(e) => ClientSendOutcome::Rejected {
                error: e.to_string(),
            },
            Ok(ClientActionResult::Disconnect) => {
                let loc = d.location.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    d.ex.request(
                        d.server,
                        codec::CODE_DELETE,
                        path_options(&loc),
                        vec![],
                        None,
                    ),
                )
                .await;
                if let Some(c) = command {
                    crate::client::command_support::reply(c, Ok(ClientSendOutcome::Disconnected));
                }
                return Ok(());
            }
            Ok(_) => match action["type"].as_str() {
                Some("lwm2m_update") => match d.update().await {
                    Ok(()) => ClientSendOutcome::Sent { bytes_sent: 0 },
                    Err(e) => ClientSendOutcome::Rejected {
                        error: format!("{e:#}"),
                    },
                },
                Some("lwm2m_notify") => {
                    let path = action["path"].as_str().unwrap_or_default().to_owned();
                    let obs = {
                        let mut all = d.observations.lock().unwrap_or_else(|e| e.into_inner());
                        all.iter_mut()
                            .find(|(p, _)| path == **p || path.starts_with(&format!("{p}/")))
                            .map(|(p, (t, seq))| {
                                *seq += 1;
                                (p.clone(), t.clone(), *seq)
                            })
                    };
                    match obs {
                        Some((_, token, seq)) => match content::senml_encode(
                            action["values"]
                                .as_array()
                                .map(Vec::as_slice)
                                .unwrap_or_default(),
                        ) {
                            Ok(payload) => match d
                                .ex
                                .notify(
                                    d.server,
                                    &token,
                                    seq,
                                    vec![uint_option(
                                        codec::OPT_CONTENT_FORMAT,
                                        content::CF_SENML_JSON as u32,
                                    )],
                                    payload,
                                )
                                .await
                            {
                                Ok(()) => ClientSendOutcome::Sent { bytes_sent: 0 },
                                Err(e) => ClientSendOutcome::Rejected {
                                    error: format!("{e:#}"),
                                },
                            },
                            Err(e) => ClientSendOutcome::Rejected {
                                error: e.to_string(),
                            },
                        },
                        None => ClientSendOutcome::Rejected {
                            error: format!("the server does not observe {path}"),
                        },
                    }
                }
                _ => ClientSendOutcome::Rejected {
                    error: "lwm2m_content, lwm2m_ok and lwm2m_error only answer a request".into(),
                },
            },
        };
        if let Some(c) = command {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "LwM2M",
                    None,
                    "injected_action",
                    json!({"type": action["type"]}),
                    vec![serde_json::to_value(&outcome).unwrap_or(Json::Null)],
                )
                .await;
            crate::client::command_support::reply(c, Ok(outcome));
        }
    }
}
