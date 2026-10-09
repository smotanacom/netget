pub mod actions;
pub mod tls;
pub mod wire;
use self::wire::Request;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::GeminiClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let config = tls::Config::from_context(&ctx)?;
    let stream = tokio::time::timeout(wire::DEADLINE, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("Gemini connect deadline")??;
    let local = stream.local_addr()?;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = GeminiClientProtocol;
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
                Err(e) => Log::new(Some(&events_ctx.status_tx))
                    .warn(format!("Gemini client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(
            &session_ctx,
            stream,
            config,
            external,
            internal_rx,
            event_tx,
        )
        .await;
        // Drain queued final responses when the logical client disconnects cleanly.
        // The dispatcher owns no socket; removal still aborts both tasks.
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx)).warn(format!("Gemini client ended: {e}"));
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
async fn session(
    ctx: &ConnectContext,
    stream: TcpStream,
    config: tls::Config,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let mut initial = Some({
        let connector = tokio_rustls::TlsConnector::from(config.tls.clone());
        let handshake = tokio::time::timeout(
            wire::DEADLINE,
            connector.connect(config.name.clone(), stream),
        );
        tokio::pin!(handshake);
        loop {
            tokio::select! {
                result=&mut handshake => break result.context("Gemini TLS handshake deadline")??,
                command=external.recv()=> {
                    let Some(command)=command else{return Ok(());};
                    if matches!(GeminiClientProtocol.execute_action(command.action.clone()),Ok(ClientActionResult::Disconnect)) {
                        crate::client::command_support::reply(command,Ok(ClientSendOutcome::Disconnected));return Ok(());
                    }
                    crate::client::command_support::reply(command,Ok(ClientSendOutcome::Rejected{error:"TLS handshake still pending".into()}));
                }
            }
        }
    });
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    events.try_send(Event::new(
        &actions::CONNECTED_EVENT,
        json!({"remote_addr":ctx.remote_addr,"server_name":config.host}),
    ))?;
    loop {
        let (action, command) = tokio::select! {
            command=external.recv()=>match command{Some(c)=>(c.action.clone(),Some(c)),None=>return Ok(())},
            action=internal.recv()=>match action{Some(a)=>(a,None),None=>return Ok(())},
        };
        let request = match GeminiClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Custom { name, data }) if name == "gemini_request" => {
                Request::from_action(&data).and_then(|r| {
                    config.validate(&r)?;
                    Ok(r)
                })
            }
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = command {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Err(e) => Err(e),
            _ => Err(anyhow::anyhow!("Unsupported action result")),
        };
        let request = match request {
            Ok(request) => request,
            Err(e) => {
                if let Some(command) = command {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        };
        let response = {
            let transaction = exchange(ctx, &config, &request, initial.take(), command, &action);
            let pending = tokio::time::timeout(wire::DEADLINE, transaction);
            tokio::pin!(pending);
            loop {
                tokio::select! {
                    biased;
                    result=&mut pending=>break result.context("Gemini transaction deadline")??,
                    command=external.recv()=>{
                        let Some(command)=command else{return Ok(());};
                        if matches!(GeminiClientProtocol.execute_action(command.action.clone()),Ok(ClientActionResult::Disconnect)) {
                            crate::client::command_support::reply(command,Ok(ClientSendOutcome::Disconnected));return Ok(());
                        }
                        crate::client::command_support::reply(command,Ok(ClientSendOutcome::Rejected{error:"Gemini transaction pending; retry after its response".into()}));
                    }
                }
            }
        };
        events
            .try_send(Event::new(
                &actions::RESPONSE_EVENT,
                json!({"request":action,"response":response}),
            ))
            .context("Gemini event consumer stalled")?;
    }
}
async fn exchange(
    ctx: &ConnectContext,
    config: &tls::Config,
    request: &Request,
    initial: Option<tokio_rustls::client::TlsStream<TcpStream>>,
    command: Option<ClientCommand>,
    action: &Value,
) -> Result<Value> {
    let mut stream = match initial {
        Some(stream) => stream,
        None => {
            let stream = TcpStream::connect(&ctx.remote_addr).await?;
            tokio_rustls::TlsConnector::from(config.tls.clone())
                .connect(config.name.clone(), stream)
                .await?
        }
    };
    let result = stream.write_all(&request.bytes).await;
    if command.is_some() {
        ctx.state
            .record_access_log(
                AccessLogOwner::Client(ctx.client_id.as_u32()),
                "Gemini",
                None,
                "injected_action",
                action.clone(),
                vec![json!({"sent":result.is_ok()})],
            )
            .await;
    }
    if let Some(command) = command {
        crate::client::command_support::reply(
            command,
            match &result {
                Ok(()) => Ok(ClientSendOutcome::Sent {
                    bytes_sent: request.bytes.len(),
                }),
                Err(e) => Err(anyhow::anyhow!(e.to_string())),
            },
        );
    }
    result?;
    let mut reader = BufReader::new(stream);
    let response = wire::response(&mut reader, request).await?;
    // Each Gemini connection carries exactly one request, even for input and redirect responses.
    reader.get_mut().shutdown().await?;
    Ok(response)
}
