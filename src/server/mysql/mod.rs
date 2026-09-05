//! MySQL server implementation using opensrv-mysql
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use actions::{MysqlProtocol, MYSQL_QUERY_EVENT};
use anyhow::Result;
use async_trait::async_trait;
use opensrv_mysql::{
    AsyncMysqlIntermediary, AsyncMysqlShim, Column, ColumnFlags, ColumnType, ErrorKind, InitWriter,
    OkResponse, ParamParser, QueryResultWriter, StatementMetaWriter, StatusFlags,
};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, trace, warn};

/// Cap on prepared statements retained per connection.
///
/// The map is keyed by statement id and only pruned by an explicit COM_STMT_CLOSE, so a client
/// that PREPAREs in a loop and never closes would grow it without bound.
const MAX_PREPARED_STATEMENTS: usize = 4096;

/// MySQL server implementation
pub struct MysqlServer {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

impl MysqlServer {
    /// Create a new MySQL server
    pub fn new(
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Self {
        Self {
            llm_client,
            app_state,
            _status_tx: status_tx,
            server_id,
        }
    }

    /// Spawn MySQL server with LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        _send_first: bool,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("MySQL server listening on {}", actual_addr));

        let server = Arc::new(MysqlServer::new(
            llm_client,
            app_state.clone(),
            status_tx.clone(),
            Some(server_id),
        ));

        let status_tx_clone = status_tx.clone();
        let task_registrar = app_state.clone();

        // Spawn the accept loop
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        Log::new(Some(&status_tx)).info(format!("MySQL connection from {}", addr));

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(actual_addr);

                        let handler = MysqlHandler::new(
                            connection_id,
                            server.llm_client.clone(),
                            server.app_state.clone(),
                            status_tx.clone(),
                            server.server_id,
                            addr,
                        );

                        // Track the connection
                        if let Some(server_id) = server.server_id {
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();
                            let conn_state = ServerConnectionState {
                                id: connection_id,
                                remote_addr: addr,
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
                            server
                                .app_state
                                .add_connection_to_server(server_id, conn_state)
                                .await;
                        }

                        let conn_state_owner = server.app_state.clone();
                        let conn_server_id = server.server_id;
                        tokio::spawn(async move {
                            // MySQL requires split read/write streams
                            let (reader, writer) = tokio::io::split(stream);
                            if let Err(e) =
                                AsyncMysqlIntermediary::run_on(handler, reader, writer).await
                            {
                                error!("MySQL connection error: {:?}", e);
                            }
                            // Mark the connection closed so it does not stay Active forever
                            // in the server's connection map.
                            if let Some(server_id) = conn_server_id {
                                conn_state_owner
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("MySQL accept error: {}", e));
                    }
                }
            }
        });

        // Register the accept loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
        Ok(actual_addr)
    }
}

/// MySQL connection handler
pub struct MysqlHandler {
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    #[allow(dead_code)]
    server_id: Option<crate::state::ServerId>,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    /// MySQL protocol handler for action execution
    protocol: Arc<MysqlProtocol>,
    /// Prepared statements
    prepared_statements: Arc<Mutex<std::collections::HashMap<u32, String>>>,
    /// Next statement ID
    next_stmt_id: Arc<Mutex<u32>>,
}

impl MysqlHandler {
    pub fn new(
        connection_id: ConnectionId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
        remote_addr: SocketAddr,
    ) -> Self {
        let protocol = Arc::new(MysqlProtocol::new(
            connection_id,
            app_state.clone(),
            status_tx.clone(),
        ));

        Self {
            connection_id,
            llm_client,
            app_state,
            status_tx,
            server_id,
            remote_addr,
            protocol,
            prepared_statements: Arc::new(Mutex::new(std::collections::HashMap::new())),
            next_stmt_id: Arc::new(Mutex::new(1)),
        }
    }
}

#[async_trait]
impl<W: tokio::io::AsyncWrite + Send + Unpin> AsyncMysqlShim<W> for MysqlHandler {
    type Error = io::Error;

    async fn on_prepare<'a>(
        &'a mut self,
        query: &'a str,
        info: StatementMetaWriter<'a, W>,
    ) -> io::Result<()> {
        Log::new(Some(&self.status_tx)).debug(format!("MySQL PREPARE: {}", query));
        self.record_stats(Some(query.len() as u64), None, Some(1), None)
            .await;

        // Store the prepared statement
        let mut next_id = self.next_stmt_id.lock().await;
        let stmt_id = *next_id;
        *next_id = next_id.wrapping_add(1);
        drop(next_id);

        let mut stmts = self.prepared_statements.lock().await;
        if stmts.len() >= MAX_PREPARED_STATEMENTS {
            drop(stmts);
            warn!(
                "MySQL connection {} exceeded {} prepared statements",
                self.connection_id, MAX_PREPARED_STATEMENTS
            );
            return info
                .error(
                    ErrorKind::ER_MAX_PREPARED_STMT_COUNT_REACHED,
                    b"Can't create more than max_prepared_stmt_count statements",
                )
                .await;
        }
        stmts.insert(stmt_id, query.to_string());
        drop(stmts);

        // Reply with the statement ID
        self.record_stats(None, Some(0), None, Some(1)).await;
        info.reply(stmt_id, &[], &[]).await
    }

    async fn on_execute<'a>(
        &'a mut self,
        stmt_id: u32,
        _params: ParamParser<'a>,
        results: QueryResultWriter<'a, W>,
    ) -> io::Result<()> {
        Log::new(Some(&self.status_tx)).debug(format!("MySQL EXECUTE statement {}", stmt_id));

        // Get the prepared statement
        let stmts = self.prepared_statements.lock().await;
        let query = stmts.get(&stmt_id).cloned();
        drop(stmts);

        if let Some(query) = query {
            // Treat as a regular query
            self.handle_query(&query, results).await
        } else {
            // The handle is unknown or already closed. Answering OK told the driver its
            // EXECUTE succeeded and affected zero rows, so a client reusing a stale handle —
            // after a reconnect, or a double-close — saw a successful write that never ran.
            // MySQL has a code for exactly this and drivers act on it.
            warn!(
                "MySQL EXECUTE for unknown or expired statement id {} (decision=unknown_handle)",
                stmt_id
            );
            results
                .error(
                    ErrorKind::ER_UNKNOWN_STMT_HANDLER,
                    format!(
                        "Unknown prepared statement handler ({}) given to EXECUTE",
                        stmt_id
                    )
                    .as_bytes(),
                )
                .await
        }
    }

    async fn on_close(&mut self, stmt_id: u32) {
        Log::new(Some(&self.status_tx)).debug(format!("MySQL CLOSE statement {}", stmt_id));

        let mut stmts = self.prepared_statements.lock().await;
        stmts.remove(&stmt_id);
    }

    async fn on_query<'a>(
        &'a mut self,
        query: &'a str,
        results: QueryResultWriter<'a, W>,
    ) -> io::Result<()> {
        // FileOnly: the mysql_query event's log_template already reports the query to the
        // TUI at INFO when call_llm dispatches it (see actions.rs).
        Log::new(Some(&self.status_tx)).debug(format!("MySQL QUERY: {}", query));

        self.handle_query(query, results).await
    }

    async fn on_init<'a>(
        &'a mut self,
        _database: &'a str,
        writer: InitWriter<'a, W>,
    ) -> io::Result<()> {
        Log::new(Some(&self.status_tx)).debug(format!("MySQL INIT DB: {}", _database));
        self.record_stats(Some(_database.len() as u64), None, Some(1), Some(1))
            .await;

        writer.ok().await
    }
}

impl MysqlHandler {
    /// Refresh the dashboard's per-connection counters and `last_activity`.
    ///
    /// opensrv-mysql owns the socket inside `run_on` and never exposes the raw byte streams, so
    /// these are the **application-visible** payload sizes seen at the shim boundary (the SQL text
    /// received, the response cells/message produced), not the exact wire bytes — opensrv adds a
    /// 4-byte packet header and text-protocol framing on top. Good enough for the rail's `↓/↑`
    /// counters and, more importantly, for keeping `last_activity` current.
    async fn record_stats(
        &self,
        bytes_in: Option<u64>,
        bytes_out: Option<u64>,
        packets_in: Option<u64>,
        packets_out: Option<u64>,
    ) {
        if let Some(server_id) = self.server_id {
            self.app_state
                .update_connection_stats(
                    server_id,
                    self.connection_id,
                    bytes_in,
                    bytes_out,
                    packets_in,
                    packets_out,
                )
                .await;
        }
    }

    async fn handle_query<'a, W: tokio::io::AsyncWrite + Send + Unpin>(
        &'a mut self,
        query: &str,
        results: QueryResultWriter<'a, W>,
    ) -> io::Result<()> {
        trace!("Calling LLM for MySQL query: {}", query);

        // The SQL text is the visible inbound payload for this command (COM_QUERY, or the replayed
        // statement text for COM_STMT_EXECUTE).
        self.record_stats(Some(query.len() as u64), None, Some(1), None)
            .await;

        // Create query event
        let event = Event::new(
            &MYSQL_QUERY_EVENT,
            serde_json::json!({
                "query": query,
            }),
        );

        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(0));

        let llm_result = call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await;

        match llm_result {
            Ok(execution_result) => {
                // `close_this_connection` is a declared sync action, so it has to actually end
                // the session. opensrv drives the loop for us, so the only way to stop it is to
                // answer the current query and then return an error from the shim.
                let close_requested = execution_result
                    .protocol_results
                    .iter()
                    .any(|r| matches!(r, ActionResult::CloseConnection));

                // Process action results to find MySQL responses
                for result in execution_result.protocol_results {
                    match result {
                        ActionResult::Custom { name, data } => {
                            match name.as_str() {
                                "mysql_query_response" => {
                                    // Extract columns and rows from JSON data
                                    let columns = data
                                        .get("columns")
                                        .and_then(|v| v.as_array())
                                        .cloned()
                                        .unwrap_or_default();
                                    let rows = data
                                        .get("rows")
                                        .and_then(|v| v.as_array())
                                        .cloned()
                                        .unwrap_or_default();

                                    // Estimate the outbound payload from the cell text before the
                                    // writers consume `columns`/`rows`.
                                    let sent_bytes: u64 = columns
                                        .iter()
                                        .filter_map(|c| c.get("name"))
                                        .filter_map(|v| v.as_str())
                                        .map(|s| s.len() as u64)
                                        .sum::<u64>()
                                        + rows
                                            .iter()
                                            .filter_map(|r| r.as_array())
                                            .flatten()
                                            .map(|v| json_to_mysql_string(v).len() as u64)
                                            .sum::<u64>();
                                    self.record_stats(None, Some(sent_bytes), None, Some(1))
                                        .await;

                                    // Send result set
                                    return finish_query(
                                        send_result_set(results, columns, rows).await,
                                        close_requested,
                                    );
                                }
                                "mysql_error" => {
                                    // Extract error info from JSON data
                                    let error_code = data
                                        .get("error_code")
                                        .and_then(|v| v.as_u64())
                                        .and_then(|c| u16::try_from(c).ok())
                                        .unwrap_or(1064);
                                    let message = data
                                        .get("message")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("Unknown error");

                                    // Send a real MySQL ERR packet. `QueryResultWriter::error`
                                    // has existed since opensrv-mysql 0.4; the previous code
                                    // (and the protocol docs) claimed the library could only
                                    // send OK, which silently swallowed every LLM error.
                                    //
                                    // Non-fatal: this is the model's own deliberate
                                    // mysql_error_response, answered on the wire and the
                                    // connection continues.
                                    Log::new(Some(&self.status_tx))
                                        .warn(format!("MySQL error {}: {}", error_code, message));
                                    self.record_stats(
                                        None,
                                        Some(message.len() as u64),
                                        None,
                                        Some(1),
                                    )
                                    .await;
                                    return finish_query(
                                        results
                                            .error(mysql_error_kind(error_code), message.as_bytes())
                                            .await,
                                        close_requested,
                                    );
                                }
                                "mysql_ok" => {
                                    // Extract OK response info from JSON data
                                    let affected_rows = data
                                        .get("affected_rows")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);
                                    let last_insert_id = data
                                        .get("last_insert_id")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0);

                                    // Send OK response (a small fixed-size packet).
                                    self.record_stats(None, Some(0), None, Some(1)).await;
                                    return finish_query(
                                        results
                                            .completed(OkResponse {
                                                header: 0,
                                                affected_rows,
                                                last_insert_id,
                                                status_flags: StatusFlags::empty(),
                                                warnings: 0,
                                                info: String::new(),
                                                session_state_info: String::new(),
                                            })
                                            .await,
                                        close_requested,
                                    );
                                }
                                _ => {
                                    // Unknown custom response, ignore
                                }
                            }
                        }
                        _ => {
                            // Other action results are informational, continue processing
                        }
                    }
                }

                // No response action matched: the handler ran but produced nothing this
                // protocol can encode — a model that refused, a static handler with an empty
                // action list, or an answer whose actions were all unrecognised.
                //
                // This used to reply with an empty OK, which a driver reads as a statement that
                // executed successfully and affected zero rows. For an INSERT, UPDATE or DELETE
                // that is a claim the write completed, and the caller carries on believing it
                // landed. The log line admitted it was "indistinguishable from a successful
                // no-op" — which is precisely why it cannot be the answer.
                //
                // Fail closed with an ERR packet instead, the same shape the backend-error arm
                // below uses. 1105 (ER_UNKNOWN_ERROR) rather than 1205: nothing here timed out,
                // so advertising a retryable condition would be wrong.
                warn!(
                    "MySQL: no response action produced for query {:?} \
                     (decision=fail_closed_no_action); replying with an ERR packet",
                    query
                );
                let message = crate::utils::WireFailure::Unavailable.prefixed_text();
                self.record_stats(None, Some(message.len() as u64), None, Some(1))
                    .await;
                finish_query(
                    results
                        .error(ErrorKind::ER_UNKNOWN_ERROR, message.as_bytes())
                        .await,
                    close_requested,
                )
            }
            Err(e) => {
                // Report the failure as a MySQL ERR packet instead of a silent empty OK, so
                // the client sees something rather than an unexplained success. The ERR packet
                // carries the error number and the SQLSTATE that goes with it, which is what
                // lets a driver classify the failure instead of guessing from a message.
                //
                // Overload gets 1205 (SQLSTATE HY000), the code every MySQL driver already
                // treats as "transient, safe to retry", rather than 1105 which reads as a
                // permanent server fault.
                let overloaded = crate::llm::is_overload_error(&e);
                let message = crate::utils::WireFailure::classify(&e).prefixed_text();
                let kind = if overloaded {
                    ErrorKind::ER_LOCK_WAIT_TIMEOUT
                } else {
                    ErrorKind::ER_UNKNOWN_ERROR
                };
                // Non-fatal: a wire fallback (ERR packet) is still delivered and the
                // connection continues.
                Log::new(Some(&self.status_tx))
                    .warn(format!("MySQL replying with error: {}", message));
                self.record_stats(None, Some(message.len() as u64), None, Some(1))
                    .await;
                results.error(kind, message.as_bytes()).await
            }
        }
    }
}

/// Map an LLM-supplied MySQL error number onto an `opensrv_mysql::ErrorKind`.
///
/// `ErrorKind::from(u16)` **panics** on any value that is not one of the ~886 codes it knows
/// (`opensrv-mysql-0.7.0/src/errorcodes.rs:2807`). The number here comes straight out of model
/// output, so calling it directly would let a hallucinated error code kill the connection task.
/// We therefore accept the error numbers a model realistically produces and fall back to
/// `ER_UNKNOWN_ERROR` (1105) for anything else.
fn mysql_error_kind(code: u16) -> ErrorKind {
    match code {
        1044 => ErrorKind::ER_DBACCESS_DENIED_ERROR,
        1045 => ErrorKind::ER_ACCESS_DENIED_ERROR,
        1046 => ErrorKind::ER_NO_DB_ERROR,
        1049 => ErrorKind::ER_BAD_DB_ERROR,
        1050 => ErrorKind::ER_TABLE_EXISTS_ERROR,
        1051 => ErrorKind::ER_BAD_TABLE_ERROR,
        1052 => ErrorKind::ER_NON_UNIQ_ERROR,
        1054 => ErrorKind::ER_BAD_FIELD_ERROR,
        1062 => ErrorKind::ER_DUP_ENTRY,
        1064 => ErrorKind::ER_PARSE_ERROR,
        1065 => ErrorKind::ER_EMPTY_QUERY,
        1136 => ErrorKind::ER_WRONG_VALUE_COUNT_ON_ROW,
        1146 => ErrorKind::ER_NO_SUCH_TABLE,
        1149 => ErrorKind::ER_SYNTAX_ERROR,
        1216 => ErrorKind::ER_NO_REFERENCED_ROW,
        1217 => ErrorKind::ER_ROW_IS_REFERENCED,
        1364 => ErrorKind::ER_NO_DEFAULT_FOR_FIELD,
        1451 => ErrorKind::ER_ROW_IS_REFERENCED_2,
        1452 => ErrorKind::ER_NO_REFERENCED_ROW_2,
        1690 => ErrorKind::ER_DATA_OUT_OF_RANGE,
        other => {
            warn!(
                "MySQL: error code {} is not in opensrv-mysql's table, reporting 1105 ER_UNKNOWN_ERROR",
                other
            );
            ErrorKind::ER_UNKNOWN_ERROR
        }
    }
}

/// Turn a successfully-written response into a connection teardown when the LLM asked for
/// `close_this_connection`. opensrv-mysql ends the session when the shim returns an error.
fn finish_query(result: io::Result<()>, close_requested: bool) -> io::Result<()> {
    match result {
        Ok(()) if close_requested => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "MySQL connection closed by close_this_connection action",
        )),
        other => other,
    }
}

/// Send a result set to the client
async fn send_result_set<'a, W: tokio::io::AsyncWrite + Send + Unpin>(
    results: QueryResultWriter<'a, W>,
    columns: Vec<serde_json::Value>,
    rows: Vec<serde_json::Value>,
) -> io::Result<()> {
    // Parse column definitions
    let mut cols = Vec::new();
    for col_def in &columns {
        let name = col_def
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("column");

        let col_type = col_def
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("VARCHAR");

        let mysql_type = match col_type.to_uppercase().as_str() {
            "INT" | "INTEGER" => ColumnType::MYSQL_TYPE_LONG,
            "BIGINT" => ColumnType::MYSQL_TYPE_LONGLONG,
            "SMALLINT" => ColumnType::MYSQL_TYPE_SHORT,
            "TINYINT" => ColumnType::MYSQL_TYPE_TINY,
            "FLOAT" => ColumnType::MYSQL_TYPE_FLOAT,
            "DOUBLE" => ColumnType::MYSQL_TYPE_DOUBLE,
            "DECIMAL" => ColumnType::MYSQL_TYPE_DECIMAL,
            "DATE" => ColumnType::MYSQL_TYPE_DATE,
            "TIME" => ColumnType::MYSQL_TYPE_TIME,
            "DATETIME" | "TIMESTAMP" => ColumnType::MYSQL_TYPE_DATETIME,
            "BLOB" | "BINARY" => ColumnType::MYSQL_TYPE_BLOB,
            "TEXT" => ColumnType::MYSQL_TYPE_STRING,
            _ => ColumnType::MYSQL_TYPE_VAR_STRING,
        };

        cols.push(Column {
            table: "".to_string(),
            column: name.to_string(),
            coltype: mysql_type,
            colflags: ColumnFlags::empty(),
        });
    }

    // Start the result set
    let mut row_writer = results.start(&cols).await?;

    // Write rows
    for row_data in &rows {
        if let Some(row_values) = row_data.as_array() {
            // Convert JSON values to Strings (simplified - ToMysqlValue is implemented for String)
            let values: Vec<String> = row_values.iter().map(|v| json_to_mysql_string(v)).collect();

            row_writer.write_row(values).await?;
        }
    }

    // Finish the result set
    row_writer.finish().await
}

/// Convert JSON value to MySQL string representation
fn json_to_mysql_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => v.to_string(),
    }
}
