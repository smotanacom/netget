//! ClickHouse native TCP client at protocol revision 54429. One connection; each action is a
//! query (or an insert) run to its end of stream.
pub mod actions;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::logging::emit::Log;
use crate::protocol::{ConnectContext, Event};
use crate::server::clickhouse::wire::{self, client_packet, server_packet, Block, Source};
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientStatus};
pub use actions::ClickhouseClientProtocol;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::{
    io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::mpsc,
};

pub const DEFAULT_USER: &str = "default";
/// How long one query may run before the client gives up on it.
const QUERY_TIMEOUT: Duration = Duration::from_secs(120);

struct Conn {
    reader: BufReader<ReadHalf<TcpStream>>,
    writer: WriteHalf<TcpStream>,
    revision: u64,
}

impl Conn {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).await?;
        Ok(())
    }

    fn empty_data() -> Result<Vec<u8>> {
        wire::data_packet(client_packet::DATA, &Block::default(), false)
    }

    /// Read server packets until end of stream. Returns the result rows (or the exception),
    /// and the header block when `stop_at_header` (an INSERT's table description).
    async fn read_until_end(
        &mut self,
        stop_at_header: bool,
    ) -> Result<std::result::Result<Block, (i32, String)>> {
        let mut result = Block::default();
        loop {
            let kind = wire::read_packet_type(&mut self.reader, QUERY_TIMEOUT)
                .await?
                .context("server closed the connection mid-query")?;
            let mut s = Source::plain(&mut self.reader);
            match kind {
                server_packet::DATA
                | server_packet::TOTALS
                | server_packet::EXTREMES
                | server_packet::LOG => {
                    s.string().await?;
                    let mut b = Source::plain(&mut self.reader);
                    let block = Block::decode(&mut b).await?;
                    if kind != server_packet::DATA {
                        continue;
                    }
                    if stop_at_header {
                        return Ok(Ok(block));
                    }
                    if result.columns.is_empty() {
                        result.columns = block.columns.clone();
                    }
                    anyhow::ensure!(
                        result.rows.len() + block.rows.len() <= wire::MAX_ROWS,
                        "result exceeds {} rows",
                        wire::MAX_ROWS
                    );
                    result.rows.extend(block.rows);
                }
                server_packet::PROGRESS => {
                    s.varuint().await?;
                    s.varuint().await?;
                    if self.revision >= 51554 {
                        s.varuint().await?;
                    }
                    if self.revision >= 54420 {
                        s.varuint().await?;
                        s.varuint().await?;
                    }
                }
                server_packet::PROFILE_INFO => {
                    s.varuint().await?;
                    s.varuint().await?;
                    s.varuint().await?;
                    s.u8().await?;
                    s.varuint().await?;
                    s.u8().await?;
                }
                server_packet::TABLE_COLUMNS => {
                    s.string().await?;
                    s.string().await?;
                }
                server_packet::EXCEPTION => return Ok(Err(wire::read_exception(&mut s).await?)),
                server_packet::END_OF_STREAM => return Ok(Ok(result)),
                other => anyhow::bail!("unexpected server packet {other}"),
            }
        }
    }

    async fn run(&mut self, action: &Value) -> Result<Value> {
        let query = action["query"].as_str().unwrap_or_default().to_string();
        let mut out = wire::query_packet(&query, false);
        out.extend(Self::empty_data()?);
        self.send(&out).await?;
        let mut event = json!({"query": query});
        if action["type"] == "clickhouse_insert" {
            let header = match self.read_until_end(true).await? {
                Ok(h) => h,
                Err((code, message)) => {
                    event["ok"] = json!(false);
                    event["exception"] = json!({"code": code, "message": message});
                    return Ok(event);
                }
            };
            let block = Block::from_json(&header.columns_json(), &action["rows"]);
            let block = match block {
                Ok(b) => b,
                Err(e) => {
                    // The server is waiting for data: end the insert with no rows.
                    self.send(&Self::empty_data()?).await?;
                    let _ = self.read_until_end(false).await?;
                    anyhow::bail!("rows do not fit the table: {e:#}");
                }
            };
            let mut out = wire::data_packet(client_packet::DATA, &block, false)?;
            out.extend(Self::empty_data()?);
            self.send(&out).await?;
            event["rows_written"] = json!(block.rows.len());
        }
        match self.read_until_end(false).await? {
            Ok(block) => {
                event["ok"] = json!(true);
                if !block.columns.is_empty() {
                    event["columns"] = block.columns_json();
                    event["rows"] =
                        Value::Array(block.rows.into_iter().map(Value::Array).collect());
                }
            }
            Err((code, message)) => {
                event["ok"] = json!(false);
                event["exception"] = json!({"code": code, "message": message});
            }
        }
        Ok(event)
    }
}

pub async fn connect(ctx: ConnectContext) -> Result<SocketAddr> {
    let params = ctx.startup_params.as_ref();
    let get = |name: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(name))
            .transpose()?
            .flatten())
    };
    let user = get("user")?.unwrap_or_else(|| DEFAULT_USER.into());
    let password = get("password")?.unwrap_or_default();
    let database = get("database")?.unwrap_or_default();
    let stream = tokio::time::timeout(wire::IO_TIMEOUT, TcpStream::connect(&ctx.remote_addr))
        .await
        .context("ClickHouse connect deadline")??;
    let local = stream.local_addr()?;
    let (reader, writer) = tokio::io::split(stream);
    let mut conn = Conn {
        reader: BufReader::new(reader),
        writer,
        revision: wire::REVISION,
    };
    let mut hello = Vec::new();
    wire::put_varuint(&mut hello, client_packet::HELLO);
    wire::put_string(&mut hello, b"NetGet");
    wire::put_varuint(&mut hello, wire::VERSION_MAJOR);
    wire::put_varuint(&mut hello, wire::VERSION_MINOR);
    wire::put_varuint(&mut hello, wire::REVISION);
    wire::put_string(&mut hello, database.as_bytes());
    wire::put_string(&mut hello, user.as_bytes());
    wire::put_string(&mut hello, password.as_bytes());
    conn.send(&hello).await?;
    let connected = tokio::time::timeout(wire::IO_TIMEOUT, async {
        let kind = wire::read_packet_type(&mut conn.reader, wire::IO_TIMEOUT)
            .await?
            .context("server closed during hello")?;
        let mut s = Source::plain(&mut conn.reader);
        if kind == server_packet::EXCEPTION {
            let (code, message) = wire::read_exception(&mut s).await?;
            anyhow::bail!("ClickHouse refused the login: Code {code}: {message}");
        }
        anyhow::ensure!(kind == server_packet::HELLO, "expected the server hello, got packet {kind}");
        let name = s.string().await?;
        let major = s.varuint().await?;
        let minor = s.varuint().await?;
        let revision = s.varuint().await?.min(wire::REVISION);
        let tz = if revision >= 54058 { Some(s.string().await?) } else { None };
        if revision >= 54372 {
            s.string().await?;
        }
        let patch = if revision >= 54401 { s.varuint().await? } else { 0 };
        anyhow::Ok(json!({"server_name": name, "version": format!("{major}.{minor}.{patch}"), "revision": revision, "timezone": tz}))
    })
    .await
    .context("ClickHouse hello deadline")??;
    conn.revision = connected["revision"].as_u64().unwrap_or(wire::REVISION);
    ctx.state
        .update_client_status(ctx.client_id, ClientStatus::Connected)
        .await;
    let external =
        crate::client::command_support::register_command_channel(&ctx.state, ctx.client_id).await;
    let (internal_tx, internal_rx) = mpsc::channel::<Value>(16);
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(16);
    event_tx.try_send(Event::new(&actions::CONNECTED_EVENT, connected))?;
    let events_ctx = ctx.clone();
    let dispatcher = tokio::spawn(async move {
        let protocol = ClickhouseClientProtocol;
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
                    .warn(format!("ClickHouse client handler: {e}")),
            }
        }
    });
    let dispatcher_abort = dispatcher.abort_handle();
    ctx.state
        .register_client_task(ctx.client_id, dispatcher)
        .await;
    let session_ctx = ctx.clone();
    let task = tokio::spawn(async move {
        let result = session(&session_ctx, conn, external, internal_rx, event_tx).await;
        if result.is_err() {
            dispatcher_abort.abort();
        }
        let status = match result {
            Ok(()) => ClientStatus::Disconnected,
            Err(e) => {
                Log::new(Some(&session_ctx.status_tx))
                    .warn(format!("ClickHouse client ended: {e}"));
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
    mut conn: Conn,
    mut external: mpsc::Receiver<ClientCommand>,
    mut internal: mpsc::Receiver<Value>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    loop {
        let (action, mut injected) = tokio::select! {
            command = external.recv() => match command {
                Some(c) => (c.action.clone(), Some(c)),
                None => return Ok(()),
            },
            action = internal.recv() => match action {
                Some(a) => (a, None),
                None => return Ok(()),
            },
        };
        match ClickhouseClientProtocol.execute_action(action.clone()) {
            Ok(ClientActionResult::Disconnect) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Disconnected),
                    );
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        }),
                    );
                }
                continue;
            }
        }
        if injected.is_some() {
            ctx.state
                .record_access_log(
                    AccessLogOwner::Client(ctx.client_id.as_u32()),
                    "ClickHouse",
                    None,
                    "injected_action",
                    json!({"query": action["query"]}),
                    vec![],
                )
                .await;
        }
        // A protocol or transport failure leaves the connection mid-stream: end the session.
        let event = match conn.run(&action).await {
            Ok(event) => event,
            Err(e) => {
                if let Some(command) = injected.take() {
                    crate::client::command_support::reply(
                        command,
                        Err(anyhow::anyhow!(e.to_string())),
                    );
                }
                return Err(e);
            }
        };
        if let Some(command) = injected.take() {
            crate::client::command_support::reply(
                command,
                Ok(ClientSendOutcome::Executed {
                    detail: event.to_string(),
                }),
            );
        }
        events
            .try_send(Event::new(&actions::RESULT_EVENT, event))
            .context("ClickHouse event queue full; consumer stalled")?;
    }
}
