//! IBM Db2 server implementation (DRDA wire protocol).
//!
//! Speaks enough DRDA/DDM that a Db2 client can complete the connection handshake
//! (EXCSAT/ACCSEC/SECCHK/ACCRDB) and execute a basic statement (EXCSQLIMM), with
//! the LLM playing the database: it decides whether a login is accepted and what
//! SQLCA a statement produces. The wire codec is [`drda`]; **no storage** exists —
//! the model answers every request.
//!
//! **Fail closed.** An LLM outage during the security check is a refusal
//! (SECCHKRM at severity ERROR), never an accept; a failure during a statement is
//! an error SQLCARD, never a success. See `CLAUDE.md` for what DRDA is and is not
//! implemented here — this is validated against spec-derived byte literals, not a
//! real Db2 driver.

pub mod actions;
pub mod drda;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::db2::actions::{Db2Protocol, DB2_CONNECT_EVENT, DB2_QUERY_EVENT};
use crate::state::app_state::AppState;

/// Db2 server.
pub struct Db2Server;

impl Db2Server {
    /// Spawn the Db2 server. Binds with `?` (bind failure surfaces as
    /// `ServerStatus::Error`) and registers the accept-loop `JoinHandle` so
    /// `stop_server` can release the socket.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Db2 server listening on {}", actual_addr));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(actual_addr);
                        debug!("Db2 connection {} from {}", connection_id, remote_addr);

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let mut handler = Db2Handler {
                            connection_id,
                            server_id,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            protocol: Arc::new(Db2Protocol::new(
                                connection_id,
                                app_state.clone(),
                                status_tx.clone(),
                            )),
                            authenticated: false,
                        };

                        let conn_owner = app_state.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handler.run(stream).await {
                                debug!("Db2 connection {} ended: {}", connection_id, e);
                            }
                            conn_owner
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                        });
                    }
                    Err(e) => {
                        error!("Db2 accept error: {}", e);
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(actual_addr)
    }
}

/// Per-connection Db2/DRDA handler.
struct Db2Handler {
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<Db2Protocol>,
    /// Set once the security check (SECCHK) has been accepted.
    authenticated: bool,
}

impl Db2Handler {
    async fn run(&mut self, stream: tokio::net::TcpStream) -> Result<()> {
        // Split read/write as the codebase requires (never clone the stream). The
        // write half is shared through an `Arc<Mutex<..>>` so the dashboard's
        // peer-command task can inject writes / a disconnect alongside the
        // reader's own replies without racing on the socket.
        let (reader, write_half) = tokio::io::split(stream);
        let write_half = Arc::new(Mutex::new(write_half));

        // Peer messaging: register a command channel for THIS connection so the
        // dashboard's "[ message this peer ]" / "[ disconnect this peer ]" rows
        // work. Injected actions go through the same central executor the LLM
        // path uses. Db2's own wire verbs are correlator-bound `Custom` results,
        // so only `close_connection` reaches the wire here (see CLAUDE.md).
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &self.app_state,
            self.server_id,
            self.connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            self.protocol.clone(),
            self.app_state.clone(),
            self.server_id,
            self.connection_id.as_u32(),
            write_half.clone(),
            self.status_tx.clone(),
        );

        let result = self.run_loop(reader, &write_half).await;

        // Every exit path — clean EOF, a read/parse error, or an injected
        // disconnect — releases the peer handle so the rail stops offering a
        // dead connection. Idempotent with the peer task's own close path.
        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        result
    }

    /// The read/dispatch/reply loop. Writes go through the shared `write_half`.
    async fn run_loop(
        &mut self,
        mut reader: tokio::io::ReadHalf<tokio::net::TcpStream>,
        write_half: &Arc<Mutex<tokio::io::WriteHalf<tokio::net::TcpStream>>>,
    ) -> Result<()> {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 8192];

        loop {
            // Ensure a full DSS is buffered before parsing.
            while buf.len() < 6 || buf.len() < drda::dss_declared_len(&buf).unwrap_or(6) {
                let n = reader.read(&mut tmp).await?;
                if n == 0 {
                    return Ok(()); // clean EOF
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() >= 2 {
                    if let Some(total) = drda::dss_declared_len(&buf) {
                        if total < 6 {
                            warn!("Db2: impossible DSS length {}, dropping connection", total);
                            return Ok(());
                        }
                    }
                }
            }

            let (parsed, consumed) = match drda::parse_dss(&buf) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Db2: DRDA parse error: {}", e);
                    return Ok(());
                }
            };
            buf.drain(0..consumed);

            self.app_state
                .update_connection_stats(
                    self.server_id,
                    self.connection_id,
                    Some(consumed as u64),
                    None,
                    Some(1),
                    None,
                )
                .await;

            let reply = self.dispatch(&parsed).await;
            if let Some(bytes) = reply {
                self.app_state
                    .update_connection_stats(
                        self.server_id,
                        self.connection_id,
                        None,
                        Some(bytes.len() as u64),
                        None,
                        Some(1),
                    )
                    .await;
                let mut writer = write_half.lock().await;
                writer.write_all(&bytes).await?;
                writer.flush().await?;
            }
        }
    }

    /// Handle one DRDA command, returning the reply bytes to send (if any).
    async fn dispatch(&mut self, req: &drda::ParsedDss) -> Option<Vec<u8>> {
        use drda::cp;
        let corr = req.correlator;
        debug!(
            "Db2: DRDA command 0x{:04X} (correlator {})",
            req.codepoint, corr
        );

        match req.codepoint {
            cp::EXCSAT => Some(self.reply_excsatrd(corr)),
            cp::ACCSEC => Some(self.reply_accsecrd(corr, &req.body)),
            cp::SECCHK => Some(self.handle_secchk(corr, &req.body).await),
            cp::ACCRDB => Some(self.reply_accrdbrm(corr)),
            cp::EXCSQLIMM | cp::EXCSQLSTT => {
                // The SQL text may be embedded (SQLSTT param) or arrive as a
                // following OBJDSS. Handle the embedded case here; otherwise wait.
                if let Some(sqlstt) = drda::find_param(&req.body, cp::SQLSTT) {
                    let sql = drda::ebcdic_to_ascii(&sqlstt);
                    Some(self.handle_query(corr, sql).await)
                } else {
                    // No embedded statement — a SQLSTT OBJDSS should follow.
                    None
                }
            }
            cp::SQLSTT => {
                let sql = drda::ebcdic_to_ascii(&req.body);
                Some(self.handle_query(corr, sql).await)
            }
            other => {
                warn!("Db2: unsupported DRDA command 0x{:04X}", other);
                // CMDNSPRM (command not supported) at severity ERROR so the client
                // gets a real reply instead of hanging.
                let inner = drda::encode_scalar(cp::SVRCOD, &drda::svrcod::ERROR.to_be_bytes());
                let rm = drda::encode_object(cp::CMDNSPRM, &inner);
                Some(drda::encode_dss(drda::DSSFMT_RPYDSS, false, corr, &rm))
            }
        }
    }

    /// EXCSAT -> EXCSATRD: advertise the server's attributes.
    fn reply_excsatrd(&self, corr: u16) -> Vec<u8> {
        use drda::cp;
        let mut inner = Vec::new();
        inner.extend_from_slice(&drda::encode_scalar_str(cp::EXTNAM, "netget-db2"));
        inner.extend_from_slice(&drda::encode_scalar_str(cp::SRVCLSNM, "QDB2/NETGET"));
        inner.extend_from_slice(&drda::encode_scalar_str(cp::SRVNAM, "NETGET"));
        inner.extend_from_slice(&drda::encode_scalar_str(cp::SRVRLSLV, "SQL11055"));
        // Echo an empty manager-level list; a real negotiation would list levels.
        inner.extend_from_slice(&drda::encode_scalar(cp::MGRLVLLS, &[]));
        let rd = drda::encode_object(cp::EXCSATRD, &inner);
        drda::encode_dss(drda::DSSFMT_RPYDSS, false, corr, &rd)
    }

    /// ACCSEC -> ACCSECRD: accept the requested security mechanism (or offer
    /// USRIDPWD = 3). Echoes the client's SECMEC when present.
    fn reply_accsecrd(&self, corr: u16, body: &[u8]) -> Vec<u8> {
        use drda::cp;
        let secmec = drda::find_param(body, cp::SECMEC).unwrap_or_else(|| vec![0x00, 0x03]);
        let inner = drda::encode_scalar(cp::SECMEC, &secmec);
        let rd = drda::encode_object(cp::ACCSECRD, &inner);
        drda::encode_dss(drda::DSSFMT_RPYDSS, false, corr, &rd)
    }

    /// SECCHK -> SECCHKRM: the LLM decides accept/reject. Fail-closed: an LLM
    /// outage is a refusal (severity ERROR), distinct in the logs from a model
    /// denial.
    async fn handle_secchk(&mut self, corr: u16, body: &[u8]) -> Vec<u8> {
        use drda::cp;
        let user_id = drda::find_param(body, cp::USRID)
            .map(|b| drda::ebcdic_to_ascii(&b))
            .unwrap_or_default();
        let rdb_name = drda::find_param(body, cp::RDBNAM)
            .map(|b| drda::ebcdic_to_ascii(&b))
            .unwrap_or_default();
        let has_password = drda::find_param(body, cp::PASSWORD)
            .map(|b| !b.is_empty())
            .unwrap_or(false);

        let event = Event::new(
            &DB2_CONNECT_EVENT,
            serde_json::json!({
                "user_id": user_id,
                "rdb_name": rdb_name,
                "has_password": has_password,
            }),
        );

        match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => match first_custom(&result.protocol_results) {
                Some(("db2_accept_connection", _)) => {
                    self.authenticated = true;
                    info!("Db2 SECCHK decision=model_accept user={user_id} rdb={rdb_name}");
                    let _ = self
                        .status_tx
                        .send(format!("→ Db2 login accepted for {user_id}"));
                    secchkrm(corr, drda::svrcod::INFO, drda::secchkcd::SUCCESS)
                }
                Some(("db2_reject_connection", data)) => {
                    let code = match data.get("sec_check_code").and_then(|v| v.as_str()) {
                        Some("userid_unknown") => drda::secchkcd::USERID_UNKNOWN,
                        Some("userid_missing") => drda::secchkcd::USERID_MISSING,
                        _ => drda::secchkcd::PASSWORD_INVALID,
                    };
                    info!("Db2 SECCHK decision=model_reject user={user_id} secchkcd=0x{code:02X}");
                    let _ = self
                        .status_tx
                        .send("↩ Db2 login denied by model".to_string());
                    secchkrm(corr, drda::svrcod::ERROR, code)
                }
                _ => {
                    // The handler ran and produced neither verdict. Refuse: silence is not
                    // consent for an authentication decision.
                    warn!(
                        "Db2 SECCHK decision=fail_closed_no_action user={user_id}: the handler \
                         produced no db2_accept_connection; refusing"
                    );
                    secchkrm(corr, drda::svrcod::ERROR, drda::secchkcd::PASSWORD_INVALID)
                }
            },
            Err(e) => {
                // Fail closed, and never an accept. The SECCHKRM here is byte-identical to the
                // no-action refusal above and differs from a model denial only in a code the
                // model chose - DRDA has no field that could carry "netget could not reach a
                // decision" - so the log is the only place the three can be told apart. That is
                // the `radius` rule, and why these tags are stable strings rather than prose.
                error!(
                    "Db2 SECCHK decision=fail_closed_llm_error user={user_id}: LLM error: {:#}",
                    e
                );
                let _ = self.status_tx.send(format!(
                    "✗ Db2 auth backend error: {}",
                    crate::utils::truncate_for_log(&e.to_string(), 200)
                ));
                secchkrm(corr, drda::svrcod::ERROR, drda::secchkcd::PASSWORD_INVALID)
            }
        }
    }

    /// ACCRDB -> ACCRDBRM: grant access to the RDB once authenticated.
    fn reply_accrdbrm(&self, corr: u16) -> Vec<u8> {
        use drda::cp;
        let severity = if self.authenticated {
            drda::svrcod::INFO
        } else {
            // Not authenticated: refuse RDB access.
            drda::svrcod::ERROR
        };
        let mut inner = Vec::new();
        inner.extend_from_slice(&drda::encode_scalar(cp::SVRCOD, &severity.to_be_bytes()));
        inner.extend_from_slice(&drda::encode_scalar_str(cp::PRDID, "NETGET0100"));
        inner.extend_from_slice(&drda::encode_scalar_str(cp::TYPDEFNAM, "QTDSQLASC"));
        // Minimal TYPDEFOVR: CCSID single-byte override left empty here.
        inner.extend_from_slice(&drda::encode_scalar(cp::TYPDEFOVR, &[]));
        let rm = drda::encode_object(cp::ACCRDBRM, &inner);
        drda::encode_dss(drda::DSSFMT_RPYDSS, false, corr, &rm)
    }

    /// Handle a statement: emit db2_query, then reply SQLCARD with the model's SQLCA.
    async fn handle_query(&mut self, corr: u16, sql: String) -> Vec<u8> {
        if !self.authenticated {
            warn!(
                "Db2 statement decision=fail_closed_not_authenticated: a statement arrived \
                 before an accepted SECCHK; replying SQLCODE -30082"
            );
            let card = drda::sqlcard_error(-30082, "08001");
            return drda::encode_dss(drda::DSSFMT_OBJDSS, false, corr, &card);
        }

        let event = Event::new(
            &DB2_QUERY_EVENT,
            serde_json::json!({
                "sql_text": sql,
                "statement_type": "execute_immediate",
            }),
        );

        match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => {
                let card = match first_custom(&result.protocol_results) {
                    Some(("db2_query_ok", data)) => {
                        let sqlcode = data.get("sqlcode").and_then(|v| v.as_i64()).unwrap_or(0);
                        debug!("Db2 statement decision=model_answer sqlcode={sqlcode}");
                        if sqlcode == 0 {
                            drda::sqlcard_success()
                        } else if sqlcode > 0 {
                            // Positive SQLCODE (e.g. 100 = not found): warning SQLCA.
                            drda::sqlcard_error(sqlcode as i32, "02000")
                        } else {
                            drda::sqlcard_error(sqlcode as i32, "42000")
                        }
                    }
                    Some(("db2_query_error", data)) => {
                        let sqlcode =
                            data.get("sqlcode").and_then(|v| v.as_i64()).unwrap_or(-104) as i32;
                        let sqlstate = data
                            .get("sqlstate")
                            .and_then(|v| v.as_str())
                            .unwrap_or("42601");
                        debug!(
                            "Db2 statement decision=model_reject sqlcode={sqlcode} \
                             sqlstate={sqlstate}"
                        );
                        drda::sqlcard_error(sqlcode, sqlstate)
                    }
                    _ => {
                        // Fail closed: no usable answer -> error SQLCA, never a success. -901
                        // is Db2's own "unexpected system error", which is what this is.
                        warn!(
                            "Db2 statement decision=fail_closed_no_action: the handler produced \
                             no db2_query_ok or db2_query_error; returning SQLCODE -901"
                        );
                        drda::sqlcard_error(-901, "58004")
                    }
                };
                drda::encode_dss(drda::DSSFMT_OBJDSS, false, corr, &card)
            }
            Err(e) => {
                // The same SQLCA as the no-action path above, for the same reason as SECCHK:
                // the SQLCA extended group that would carry message text is sent NULL, so the
                // wire cannot carry the distinction and the log has to.
                error!(
                    "Db2 statement decision=fail_closed_llm_error: LLM error: {:#}",
                    e
                );
                let _ = self.status_tx.send(format!(
                    "✗ Db2 query backend error: {}",
                    crate::utils::truncate_for_log(&e.to_string(), 200)
                ));
                let card = drda::sqlcard_error(-901, "58004");
                drda::encode_dss(drda::DSSFMT_OBJDSS, false, corr, &card)
            }
        }
    }
}

/// Build a SECCHKRM (security-check reply message) with the given severity and
/// security-check code.
fn secchkrm(corr: u16, svrcod: u16, secchkcd: u8) -> Vec<u8> {
    use drda::cp;
    let mut inner = Vec::new();
    inner.extend_from_slice(&drda::encode_scalar(cp::SVRCOD, &svrcod.to_be_bytes()));
    inner.extend_from_slice(&drda::encode_scalar(cp::SECCHKCD, &[secchkcd]));
    let rm = drda::encode_object(cp::SECCHKRM, &inner);
    drda::encode_dss(drda::DSSFMT_RPYDSS, false, corr, &rm)
}

/// Find the first `Custom` action result, returning its name and data.
fn first_custom(results: &[ActionResult]) -> Option<(&str, &serde_json::Value)> {
    results.iter().find_map(|r| match r {
        ActionResult::Custom { name, data } => Some((name.as_str(), data)),
        _ => None,
    })
}
