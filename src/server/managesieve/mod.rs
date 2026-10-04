//! ManageSieve (RFC 5804) server. Rust owns the line and literal grammar, capabilities, SASL
//! PLAIN and name checks; the handler decides logins and answers every script command.
pub mod actions;
pub mod proto;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::logging::emit::Log;
use crate::protocol::{Event, SpawnContext};
use crate::server::accept_bounded::{accept_bounded, ConnectionLimiter, DEFAULT_MAX_CONNECTIONS};
use crate::server::connection::ConnectionId;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use crate::utils::clock::Instant;
use anyhow::Result;
use base64::Engine;
use proto::Arg;
use serde_json::{json, Value as Json};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_EXTENSIONS: &str =
    "fileinto reject envelope vacation imap4flags variables body copy";
pub const DEFAULT_IMPLEMENTATION: &str = "NetGet";
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Once a command has started, the rest of it (a literal included) must arrive within this.
pub const COMMAND_DEADLINE: Duration = Duration::from_secs(60);
pub const MAX_AUTH_FAILURES: u32 = 3;

struct Shared {
    ctx: SpawnContext,
    capabilities: Vec<u8>,
}

pub async fn spawn(ctx: SpawnContext) -> Result<SocketAddr> {
    let mut extensions = DEFAULT_EXTENSIONS.to_owned();
    let mut implementation = DEFAULT_IMPLEMENTATION.to_owned();
    if let Some(p) = ctx.startup_params.as_ref() {
        if let Some(e) = p.get_optional_string("sieve_extensions")? {
            anyhow::ensure!(
                e.len() <= 1024 && !e.contains(['"', '\\', '\r', '\n']),
                "sieve_extensions is plain text up to 1024 bytes"
            );
            extensions = e;
        }
        if let Some(i) = p.get_optional_string("implementation")? {
            anyhow::ensure!(
                i.len() <= 256 && !i.contains(['"', '\\', '\r', '\n']),
                "implementation is plain text up to 256 bytes"
            );
            implementation = i;
        }
    }
    let capabilities = format!(
        "\"IMPLEMENTATION\" \"{implementation}\"\r\n\"SASL\" \"PLAIN\"\r\n\"SIEVE\" \"{extensions}\"\r\n\"VERSION\" \"1.0\"\r\n"
    )
    .into_bytes();
    let listener = tokio::net::TcpListener::bind(ctx.legacy_listen_addr()).await?;
    let local = listener.local_addr()?;
    Log::new(Some(&ctx.status_tx)).info(format!("ManageSieve server on {local}"));
    let shared = Arc::new(Shared {
        ctx: ctx.clone(),
        capabilities,
    });
    let server_id = ctx.server_id;
    let accept = tokio::spawn(async move {
        let limiter = ConnectionLimiter::new(DEFAULT_MAX_CONNECTIONS);
        loop {
            let (stream, peer, permit) = match accept_bounded(
                &listener,
                &limiter,
                b"BYE \"Too many connections\"\r\n",
                "ManageSieve",
                Some(&shared.ctx.status_tx),
            )
            .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let id = ConnectionId::new(shared.ctx.state.get_next_unified_id().await);
            let now = Instant::now();
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
                    let mut s = Session {
                        shared: &child,
                        id,
                        stream,
                        buf: Vec::new(),
                        user: None,
                        failures: 0,
                    };
                    if let Err(e) = s.run().await {
                        Log::new(Some(&child.ctx.status_tx))
                            .debug(format!("ManageSieve connection {id}: {e:#}"));
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
    let summary = format!("ManageSieve connection {id} operation={operation} decision={decision}");
    let log = Log::new(Some(&ctx.status_tx));
    if decision.starts_with("fail_closed_") || decision == "model_silent" {
        log.error(summary);
    } else {
        log.info(summary);
    }
}

struct Session<'a> {
    shared: &'a Shared,
    id: ConnectionId,
    stream: TcpStream,
    buf: Vec<u8>,
    user: Option<String>,
    failures: u32,
}

impl Session<'_> {
    async fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await?;
        let ctx = &self.shared.ctx;
        ctx.state
            .update_connection_stats(
                ctx.server_id,
                self.id,
                None,
                Some(bytes.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    /// The next command line; None when the client closed or went idle.
    async fn line(&mut self) -> Result<Option<Vec<Arg>>> {
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            if let Some((args, used)) = proto::parse(&self.buf)? {
                self.buf.drain(..used);
                let ctx = &self.shared.ctx;
                ctx.state
                    .update_connection_stats(
                        ctx.server_id,
                        self.id,
                        Some(used as u64),
                        None,
                        Some(1),
                        None,
                    )
                    .await;
                return Ok(Some(args));
            }
            let limit = *deadline.get_or_insert_with(|| {
                tokio::time::Instant::now()
                    + if self.buf.is_empty() {
                        IDLE_TIMEOUT
                    } else {
                        COMMAND_DEADLINE
                    }
            });
            let mut chunk = [0u8; 16 * 1024];
            let n = match tokio::time::timeout_at(limit, self.stream.read(&mut chunk)).await {
                Ok(r) => r?,
                Err(_) if self.buf.is_empty() => {
                    self.write(&proto::status("BYE", None, "Idle timeout"))
                        .await
                        .ok();
                    return Ok(None);
                }
                Err(_) => anyhow::bail!("a command stalled"),
            };
            if n == 0 {
                return Ok(None);
            }
            if self.buf.is_empty() {
                deadline = Some(tokio::time::Instant::now() + COMMAND_DEADLINE);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    async fn ask(&mut self, event: Event, operation: &str) -> Result<Json, ()> {
        let ctx = &self.shared.ctx;
        let result = match call_llm(
            &ctx.llm_client,
            &ctx.state,
            ctx.server_id,
            Some(self.id),
            &event,
            &actions::ManageSieveProtocol,
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                outcome(ctx, self.id, operation, "fail_closed_llm_error");
                return Err(());
            }
        };
        let mut answers = Vec::new();
        let mut pending = result.protocol_results;
        while let Some(r) = pending.pop() {
            match r {
                ActionResult::Custom { data, .. } => answers.push(data),
                ActionResult::Multiple(items) => pending.extend(items),
                _ => {}
            }
        }
        match (result.failures.is_empty(), answers.len()) {
            (true, 1) => Ok(answers.remove(0)),
            (true, 0) => {
                outcome(ctx, self.id, operation, "model_silent");
                Err(())
            }
            _ => {
                outcome(ctx, self.id, operation, "fail_closed_invalid_reply");
                Err(())
            }
        }
    }

    async fn run(&mut self) -> Result<()> {
        let mut greeting = self.shared.capabilities.clone();
        greeting.extend(proto::status("OK", None, "NetGet ManageSieve ready."));
        self.write(&greeting).await?;
        loop {
            let args = match self.line().await {
                Ok(Some(a)) => a,
                Ok(None) => return Ok(()),
                Err(e) => {
                    outcome(&self.shared.ctx, self.id, "parse", "protocol_refusal");
                    self.write(&proto::status("BYE", None, "Protocol error"))
                        .await
                        .ok();
                    return Err(e);
                }
            };
            let Some(Arg::Atom(command)) = args.first() else {
                self.write(&proto::status("NO", None, "Expected a command"))
                    .await?;
                continue;
            };
            let command = command.to_ascii_uppercase();
            let rest = &args[1..];
            match command.as_str() {
                "CAPABILITY" => {
                    let mut out = self.shared.capabilities.clone();
                    out.extend(proto::status("OK", None, "Capability completed."));
                    self.write(&out).await?;
                }
                "NOOP" => {
                    let reply = match rest.first().and_then(Arg::bytes) {
                        Some(tag) => {
                            let mut code = b"TAG ".to_vec();
                            code.extend(proto::string(tag, false));
                            proto::status("OK", Some(&String::from_utf8_lossy(&code)), "Done")
                        }
                        None => proto::status("OK", None, "Done"),
                    };
                    self.write(&reply).await?;
                }
                "LOGOUT" => {
                    self.write(&proto::status("OK", None, "Logout completed."))
                        .await?;
                    return Ok(());
                }
                "STARTTLS" => {
                    self.write(&proto::status("NO", None, "STARTTLS is not available"))
                        .await?
                }
                "AUTHENTICATE" => {
                    if !self.authenticate(rest).await? {
                        return Ok(());
                    }
                }
                "UNAUTHENTICATE" if self.user.is_some() => {
                    self.user = None;
                    self.write(&proto::status("OK", None, "Unauthenticate completed."))
                        .await?;
                }
                "LISTSCRIPTS" | "GETSCRIPT" | "PUTSCRIPT" | "CHECKSCRIPT" | "SETACTIVE"
                | "DELETESCRIPT" | "RENAMESCRIPT" | "HAVESPACE" => match self.user.clone() {
                    None => {
                        self.write(&proto::status("NO", None, "Authenticate first"))
                            .await?
                    }
                    Some(user) => self.script_command(&user, &command, rest).await?,
                },
                _ => {
                    self.write(&proto::status("NO", None, "Unknown command"))
                        .await?
                }
            }
        }
    }

    /// Ok(false) when the connection should close.
    async fn authenticate(&mut self, args: &[Arg]) -> Result<bool> {
        if self.user.is_some() {
            self.write(&proto::status("NO", None, "Already authenticated"))
                .await?;
            return Ok(true);
        }
        let mechanism = args.first().and_then(Arg::text).unwrap_or_default();
        if !mechanism.eq_ignore_ascii_case("PLAIN") {
            self.write(&proto::status("NO", None, "Unsupported SASL mechanism"))
                .await?;
            return Ok(true);
        }
        let response = match args.get(1) {
            Some(r) => r.text(),
            None => {
                self.write(b"\"\"\r\n").await?;
                match self.line().await? {
                    Some(a) => a.first().and_then(Arg::text),
                    None => return Ok(false),
                }
            }
        };
        let Some(response) = response else {
            self.write(&proto::status("NO", None, "Expected a SASL response"))
                .await?;
            return Ok(true);
        };
        if response == "*" {
            self.write(&proto::status("NO", None, "Authentication cancelled"))
                .await?;
            return Ok(true);
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(response.trim())
            .ok();
        let parts: Option<Vec<String>> = decoded.and_then(|d| {
            let p: Vec<&[u8]> = d.split(|b| *b == 0).collect();
            (p.len() == 3).then(|| {
                p.iter()
                    .map(|x| String::from_utf8_lossy(x).into_owned())
                    .collect()
            })
        });
        let Some(parts) = parts.filter(|p| !p[1].is_empty()) else {
            outcome(&self.shared.ctx, self.id, "auth", "protocol_refusal");
            self.write(&proto::status("NO", None, "Malformed SASL PLAIN response"))
                .await?;
            return Ok(true);
        };
        let mut data = json!({"user": parts[1], "password": parts[2]});
        if !parts[0].is_empty() && parts[0] != parts[1] {
            data["authorize_as"] = json!(parts[0]);
        }
        let answer = self
            .ask(Event::new(&actions::AUTH_EVENT, data), "auth")
            .await;
        match answer {
            Ok(a) if a["type"] == "managesieve_ok" => {
                outcome(&self.shared.ctx, self.id, "auth", "model_answer");
                self.user = Some(if parts[0].is_empty() {
                    parts[1].clone()
                } else {
                    parts[0].clone()
                });
                self.write(&proto::status(
                    "OK",
                    None,
                    a["message"].as_str().unwrap_or("Logged in."),
                ))
                .await?;
            }
            other => {
                if other.is_ok() {
                    outcome(&self.shared.ctx, self.id, "auth", "model_reject");
                }
                self.failures += 1;
                if self.failures >= MAX_AUTH_FAILURES {
                    self.write(&proto::status("BYE", None, "Too many failed logins"))
                        .await?;
                    return Ok(false);
                }
                let code = if other.is_err() {
                    Some("TRYLATER")
                } else {
                    None
                };
                self.write(&proto::status("NO", code, "Authentication failed"))
                    .await?;
            }
        }
        Ok(true)
    }

    async fn script_command(&mut self, user: &str, command: &str, args: &[Arg]) -> Result<()> {
        let (want, names, script, size): (usize, usize, bool, bool) = match command {
            "LISTSCRIPTS" => (0, 0, false, false),
            "GETSCRIPT" | "DELETESCRIPT" | "SETACTIVE" => (1, 1, false, false),
            "PUTSCRIPT" => (2, 1, true, false),
            "CHECKSCRIPT" => (1, 0, true, false),
            "RENAMESCRIPT" => (2, 2, false, false),
            _ => (2, 1, false, true),
        };
        let syntax = |m: &str| proto::status("NO", None, m);
        if args.len() != want || args[..names].iter().any(|a| a.bytes().is_none()) {
            return self
                .write(&syntax(&format!("{command} takes {want} argument(s)")))
                .await;
        }
        let mut data = json!({"user": user, "command": command});
        for (i, key) in ["name", "new_name"].iter().enumerate().take(names) {
            let raw = args[i].bytes().unwrap_or_default();
            let deactivate = command == "SETACTIVE" && raw.is_empty();
            if !deactivate && !proto::valid_name(raw) {
                outcome(&self.shared.ctx, self.id, command, "protocol_refusal");
                return self.write(&syntax("Invalid script name")).await;
            }
            data[*key] = json!(String::from_utf8_lossy(raw));
        }
        if script {
            match args
                .last()
                .and_then(Arg::bytes)
                .map(|b| String::from_utf8(b.to_vec()))
            {
                Some(Ok(text)) => data["script"] = json!(text),
                _ => {
                    outcome(&self.shared.ctx, self.id, command, "protocol_refusal");
                    return self
                        .write(&syntax("The script is not a UTF-8 string"))
                        .await;
                }
            }
        }
        if size {
            match args.get(1) {
                Some(Arg::Number(n)) => data["size"] = json!(n),
                _ => {
                    return self
                        .write(&syntax("HAVESPACE takes a name and a number"))
                        .await
                }
            }
        }
        let operation = command.to_ascii_lowercase();
        let answer = self
            .ask(Event::new(&actions::COMMAND_EVENT, data), &operation)
            .await;
        let ctx = &self.shared.ctx;
        let reply = match answer {
            Err(()) => proto::status("NO", Some("TRYLATER"), "The server cannot answer right now"),
            Ok(a) => match (a["type"].as_str().unwrap_or_default(), command) {
                ("managesieve_no", _) => {
                    outcome(ctx, self.id, &operation, "model_reject");
                    proto::status(
                        "NO",
                        a["code"].as_str(),
                        a["message"].as_str().unwrap_or("Failed"),
                    )
                }
                ("managesieve_scripts", "LISTSCRIPTS") => {
                    outcome(ctx, self.id, &operation, "model_answer");
                    let mut out = Vec::new();
                    for s in a["scripts"].as_array().into_iter().flatten() {
                        out.extend(proto::string(
                            s["name"].as_str().unwrap_or_default().as_bytes(),
                            false,
                        ));
                        if s["active"] == true {
                            out.extend(b" ACTIVE");
                        }
                        out.extend(b"\r\n");
                    }
                    out.extend(proto::status("OK", None, "Listscripts completed."));
                    out
                }
                ("managesieve_script", "GETSCRIPT") => {
                    outcome(ctx, self.id, &operation, "model_answer");
                    let text = a["script"].as_str().unwrap_or_default();
                    let mut out = format!("{{{}}}\r\n", text.len()).into_bytes();
                    out.extend(text.as_bytes());
                    out.extend(b"\r\n");
                    out.extend(proto::status("OK", None, "Getscript completed."));
                    out
                }
                ("managesieve_ok", c) if c != "LISTSCRIPTS" && c != "GETSCRIPT" => {
                    outcome(ctx, self.id, &operation, "model_answer");
                    let code = (a["warnings"] == true).then_some("WARNINGS");
                    let default = format!("{} completed.", c.to_ascii_lowercase());
                    proto::status("OK", code, a["message"].as_str().unwrap_or(&default))
                }
                _ => {
                    outcome(ctx, self.id, &operation, "fail_closed_invalid_reply");
                    proto::status("NO", Some("TRYLATER"), "The server cannot answer right now")
                }
            },
        };
        self.write(&reply).await
    }
}
