//! ClickHouse native TCP server. Rust owns the handshake, revision negotiation, packet and
//! block encoding, compression, the INSERT exchange and every bound; the handler answers each
//! query and decides each INSERT's rows.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType, SpawnContext};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
};
use wire::{client_packet, server_packet, Block, Source};

pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_IDLE_TIMEOUT_SECS: u64 = 86400;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const AUTHENTICATION_FAILED: i32 = 516;
const UNKNOWN_EXCEPTION: i32 = 1002;
const TOO_MANY_SIMULTANEOUS_QUERIES: i32 = 202;
const UNKNOWN_PACKET_FROM_CLIENT: i32 = 101;

#[derive(Clone)]
struct Config {
    credentials: Option<(String, String)>,
    idle: Duration,
}

fn config(ctx: &SpawnContext) -> Result<Config> {
    let params = ctx.startup_params.as_ref();
    let get = |name: &str| -> Result<Option<String>> {
        Ok(params
            .map(|p| p.get_optional_string(name))
            .transpose()?
            .flatten())
    };
    let credentials = match (get("user")?, get("password")?) {
        (Some(u), Some(p)) => Some((u, p)),
        (None, None) => None,
        _ => anyhow::bail!("user and password are configured together"),
    };
    let secs = params
        .map(|p| p.get_optional_u64("idle_timeout_secs"))
        .transpose()?
        .flatten()
        .unwrap_or(IDLE_TIMEOUT.as_secs());
    anyhow::ensure!(
        (1..=MAX_IDLE_TIMEOUT_SECS).contains(&secs),
        "idle_timeout_secs must be between 1 and {MAX_IDLE_TIMEOUT_SECS}"
    );
    Ok(Config {
        credentials,
        idle: Duration::from_secs(secs),
    })
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let cfg = config(&ctx)?;
    let listener = TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let addr = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("ClickHouse native server listening on {addr}"));
    let state = ctx.state.clone();
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(
            crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS,
        );
        loop {
            let (socket, peer, permit) = match crate::server::accept_bounded::accept_bounded(
                &listener,
                &limiter,
                b"",
                "ClickHouse",
                Some(&ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(ctx.state.get_next_unified_id().await);
            let now = crate::utils::clock::Instant::now();
            ctx.state
                .add_connection_to_server(
                    server_id,
                    ConnectionState {
                        id,
                        remote_addr: peer,
                        local_addr: addr,
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
            let cfg = cfg.clone();
            ctx.state
                .spawn_server_task(server_id, async move {
                    let _permit = permit;
                    if let Err(e) = session(&child, id, socket, peer, &cfg).await {
                        Log::new(Some(&child.status_tx))
                            .warn(format!("ClickHouse connection {id} ended: {e}"));
                    }
                    child
                        .state
                        .update_connection_status(server_id, id, ConnectionStatus::Closed)
                        .await;
                    let _ = child.status_tx.send("__UPDATE_UI__".into());
                })
                .await;
        }
    });
    state.register_server_task(server_id, accept).await;
    Ok(addr)
}

fn outcome(ctx: &SpawnContext, id: ConnectionId, operation: &str, decision: &str) {
    let summary = format!("ClickHouse connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

/// The handler's answer, or the exception packet to send instead (already logged).
async fn decide(
    ctx: &SpawnContext,
    id: ConnectionId,
    event_type: &'static EventType,
    data: Value,
    allowed: &[&str],
) -> std::result::Result<(String, Value), Vec<u8>> {
    let op = event_type.id.clone();
    let generic = || {
        wire::exception(
            UNKNOWN_EXCEPTION,
            crate::utils::WireFailure::Unavailable.prefixed_text(),
        )
    };
    let result = match call_llm(
        &ctx.llm_client,
        &ctx.state,
        ctx.server_id,
        Some(id),
        &event_type_event(event_type, data),
        &actions::ClickhouseProtocol,
    )
    .await
    {
        Ok(r) => r,
        Err(error) => {
            outcome(ctx, id, &op, "fail_closed_llm_error");
            let code = if crate::llm::is_overload_error(&error) {
                TOO_MANY_SIMULTANEOUS_QUERIES
            } else {
                UNKNOWN_EXCEPTION
            };
            return Err(wire::exception(
                code,
                crate::utils::wire_failure::prefixed_wire_failure_text(&error),
            ));
        }
    };
    if !result.failures.is_empty() {
        outcome(ctx, id, &op, "fail_closed_invalid_reply");
        return Err(generic());
    }
    let mut found = Vec::new();
    let mut pending = result.protocol_results;
    while let Some(r) = pending.pop() {
        match r {
            ActionResult::Custom { name, data } if name.starts_with("clickhouse_") => {
                found.push((name, data))
            }
            ActionResult::Multiple(items) => pending.extend(items),
            _ => {}
        }
    }
    match found.len() {
        0 => {
            outcome(ctx, id, &op, "model_silent");
            Err(generic())
        }
        1 => {
            let (name, data) = found.remove(0);
            if name == "clickhouse_exception" {
                outcome(ctx, id, &op, "model_reject");
                let code = data["code"]
                    .as_i64()
                    .unwrap_or(i64::from(UNKNOWN_EXCEPTION)) as i32;
                Err(wire::exception(
                    code,
                    data["message"].as_str().unwrap_or_default(),
                ))
            } else if allowed.contains(&name.as_str()) {
                outcome(ctx, id, &op, "model_answer");
                Ok((name, data))
            } else {
                outcome(ctx, id, &op, "fail_closed_invalid_reply");
                Err(generic())
            }
        }
        _ => {
            outcome(ctx, id, &op, "fail_closed_invalid_reply");
            Err(generic())
        }
    }
}

fn event_type_event(event_type: &'static EventType, data: Value) -> Event {
    Event::new(event_type, data)
}

struct Conn<'a> {
    ctx: &'a SpawnContext,
    id: ConnectionId,
    reader: BufReader<ReadHalf<TcpStream>>,
    writer: WriteHalf<TcpStream>,
}

impl Conn<'_> {
    async fn send(&mut self, bytes: &[u8]) -> Result<()> {
        tokio::time::timeout(WRITE_TIMEOUT, self.writer.write_all(bytes))
            .await
            .context("ClickHouse write deadline")??;
        self.ctx
            .state
            .update_connection_stats(
                self.ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    /// Read a client Data packet (after its type): the table name, then the block.
    async fn read_data(&mut self, compressed: bool) -> Result<Block> {
        let mut plain = Source::plain(&mut self.reader);
        let table = plain.string().await?;
        anyhow::ensure!(table.is_empty(), "external tables are not supported");
        let mut source = Source::new(&mut self.reader, compressed);
        let block = Block::decode(&mut source).await?;
        anyhow::ensure!(
            source.drained(),
            "a compressed frame carried bytes past its block"
        );
        Ok(block)
    }

    async fn expect_data(&mut self, compressed: bool, idle: Duration) -> Result<Block> {
        let kind = wire::read_packet_type(&mut self.reader, idle)
            .await?
            .context("client closed before sending its data")?;
        anyhow::ensure!(
            kind == client_packet::DATA,
            "expected a Data packet, got {kind}"
        );
        tokio::time::timeout(wire::IO_TIMEOUT, self.read_data(compressed))
            .await
            .context("ClickHouse block deadline")?
    }
}

fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn session(
    ctx: &SpawnContext,
    id: ConnectionId,
    socket: TcpStream,
    peer: SocketAddr,
    cfg: &Config,
) -> Result<()> {
    let (reader, writer) = tokio::io::split(socket);
    let mut c = Conn {
        ctx,
        id,
        reader: BufReader::new(reader),
        writer,
    };
    let Some(kind) = wire::read_packet_type(&mut c.reader, cfg.idle).await? else {
        return Ok(());
    };
    anyhow::ensure!(
        kind == client_packet::HELLO,
        "first packet {kind} is not a hello"
    );
    let (client_name, client_revision, database, user, password) =
        tokio::time::timeout(wire::IO_TIMEOUT, async {
            let mut s = Source::plain(&mut c.reader);
            let name = s.string().await?;
            s.varuint().await?;
            s.varuint().await?;
            let revision = s.varuint().await?;
            anyhow::Ok((
                name,
                revision,
                s.string().await?,
                s.string().await?,
                s.raw_string().await?,
            ))
        })
        .await
        .context("ClickHouse hello deadline")??;
    let revision = client_revision.min(wire::REVISION);
    if let Some((u, p)) = &cfg.credentials {
        if user != *u || !same_secret(&password, p.as_bytes()) {
            c.send(&wire::exception(AUTHENTICATION_FAILED, &format!("{user}: Authentication failed: password is incorrect, or there is no user with such name."))).await?;
            return Ok(());
        }
    }
    let mut hello = Vec::new();
    wire::put_varuint(&mut hello, server_packet::HELLO);
    wire::put_string(&mut hello, b"ClickHouse");
    wire::put_varuint(&mut hello, wire::VERSION_MAJOR);
    wire::put_varuint(&mut hello, wire::VERSION_MINOR);
    wire::put_varuint(&mut hello, wire::REVISION);
    if revision >= 54058 {
        wire::put_string(&mut hello, b"UTC");
    }
    if revision >= 54372 {
        wire::put_string(&mut hello, b"netget");
    }
    if revision >= 54401 {
        wire::put_varuint(&mut hello, wire::VERSION_PATCH);
    }
    c.send(&hello).await?;
    Log::new(Some(&ctx.status_tx)).info(format!(
        "ClickHouse connection {id}: {client_name} as {user} (revision {revision})"
    ));
    loop {
        let Some(kind) = wire::read_packet_type(&mut c.reader, cfg.idle).await? else {
            return Ok(());
        };
        match kind {
            client_packet::PING => {
                let mut pong = Vec::new();
                wire::put_varuint(&mut pong, server_packet::PONG);
                c.send(&pong).await?;
            }
            client_packet::CANCEL => {}
            client_packet::QUERY => {
                let q = tokio::time::timeout(wire::IO_TIMEOUT, async {
                    let mut s = Source::plain(&mut c.reader);
                    wire::read_query(&mut s, revision).await
                })
                .await
                .context("ClickHouse query deadline")??;
                ctx.state
                    .update_connection_stats(
                        ctx.server_id,
                        id,
                        Some(q.query.len() as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;
                // The external tables end with an empty block, which always follows a query.
                let external = c.expect_data(q.compression, wire::IO_TIMEOUT).await?;
                if !external.is_empty() {
                    c.send(&wire::exception(
                        UNKNOWN_PACKET_FROM_CLIENT,
                        "external tables are not supported",
                    ))
                    .await?;
                    continue;
                }
                query(&mut c, &q, &database, &user, peer, cfg).await?;
            }
            other => {
                c.send(&wire::exception(
                    UNKNOWN_PACKET_FROM_CLIENT,
                    &format!("Unknown packet {other} from client"),
                ))
                .await?;
                return Ok(());
            }
        }
    }
}

async fn query(
    c: &mut Conn<'_>,
    q: &wire::Query,
    database: &str,
    user: &str,
    peer: SocketAddr,
    cfg: &Config,
) -> Result<()> {
    let (ctx, id) = (c.ctx, c.id);
    let settings: Map<String, Value> = q
        .settings
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let answer = decide(
        ctx,
        id,
        &actions::QUERY_EVENT,
        json!({"query": q.query, "query_id": q.query_id, "database": database, "user": user, "settings": settings, "remote_addr": peer.to_string()}),
        &["clickhouse_result", "clickhouse_ok", "clickhouse_insert"],
    )
    .await;
    let (name, data) = match answer {
        Ok(a) => a,
        Err(packet) => return c.send(&packet).await,
    };
    let mut eos = Vec::new();
    wire::put_varuint(&mut eos, server_packet::END_OF_STREAM);
    match name.as_str() {
        "clickhouse_result" => {
            let block = Block::from_json(&data["columns"], &data["rows"])?;
            let header = Block {
                columns: block.columns.clone(),
                rows: Vec::new(),
            };
            let mut out = wire::data_packet(server_packet::DATA, &header, q.compression)?;
            if !block.rows.is_empty() {
                out.extend(wire::data_packet(
                    server_packet::DATA,
                    &block,
                    q.compression,
                )?);
            }
            out.extend(wire::progress(block.rows.len() as u64, 0, 0));
            out.extend(eos);
            c.send(&out).await
        }
        "clickhouse_ok" => c.send(&eos).await,
        _ => {
            // INSERT: send the header, read row blocks until the empty one, then ask.
            let header = Block::from_json(&data["columns"], &Value::Null)?;
            c.send(&wire::data_packet(
                server_packet::DATA,
                &header,
                q.compression,
            )?)
            .await?;
            let mut rows: Vec<Value> = Vec::new();
            let mut columns = header.columns_json();
            loop {
                let block = c.expect_data(q.compression, cfg.idle).await?;
                if block.is_empty() {
                    break;
                }
                anyhow::ensure!(
                    rows.len() + block.rows.len() <= wire::MAX_ROWS,
                    "INSERT of more than {} rows",
                    wire::MAX_ROWS
                );
                columns = block.columns_json();
                rows.extend(block.rows.into_iter().map(Value::Array));
            }
            let decided = decide(
                ctx,
                id,
                &actions::INSERT_DATA_EVENT,
                json!({"query": q.query, "columns": columns, "rows": rows, "user": user}),
                &["clickhouse_ok"],
            )
            .await;
            match decided {
                Ok(_) => {
                    let mut out = wire::progress(0, 0, rows.len() as u64);
                    out.extend(eos);
                    c.send(&out).await
                }
                Err(packet) => c.send(&packet).await,
            }
        }
    }
}
