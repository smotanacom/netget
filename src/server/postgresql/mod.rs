//! PostgreSQL server implementation using pgwire
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::{console_debug, console_error};
use actions::{PostgresqlProtocol, POSTGRESQL_QUERY_EVENT};
use anyhow::Result;
use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::auth::StartupHandler;
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::StoredStatement;
use pgwire::api::{ClientInfo, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::tokio::process_socket;
use pgwire::types::format::FormatOptions;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as TokioMutex};
use tracing::{debug, error, info, warn};

/// PostgreSQL server implementation
pub struct PostgresqlServer {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    #[allow(dead_code)]
    status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

impl PostgresqlServer {
    /// Create a new PostgreSQL server
    pub fn new(
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
    ) -> Self {
        Self {
            llm_client,
            app_state,
            status_tx,
            server_id,
        }
    }

    /// Spawn PostgreSQL server with LLM integration
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

        info!("PostgreSQL server starting on {}", actual_addr);
        let _ = status_tx.send(format!(
            "[INFO] PostgreSQL server listening on {}",
            actual_addr
        ));

        let server = Arc::new(PostgresqlServer::new(
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
                        console_debug!(status_tx, "PostgreSQL connection from {}", addr);

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(actual_addr);

                        let handler_factory = Arc::new(PostgresqlHandlerFactory {
                            connection_id,
                            llm_client: server.llm_client.clone(),
                            app_state: server.app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id: server.server_id,
                            remote_addr: addr,
                            describe_cache: Arc::new(TokioMutex::new(Vec::new())),
                        });

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
                            if let Err(e) = process_socket(stream, None, handler_factory).await {
                                error!("PostgreSQL connection error: {:?}", e);
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
                        console_error!(status_tx, "PostgreSQL accept error: {}", e);
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
        Ok(actual_addr)
    }
}

/// No-auth startup handler for PostgreSQL
struct PostgresqlNoopHandler;

// Implement NoopStartupHandler trait
// StartupHandler is automatically implemented for types implementing NoopStartupHandler
#[async_trait::async_trait]
impl NoopStartupHandler for PostgresqlNoopHandler {}

/// Factory for creating PostgreSQL handlers
struct PostgresqlHandlerFactory {
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
    remote_addr: SocketAddr,
    /// Shared between the simple and extended handlers so a Describe resolved by one is
    /// visible to the Execute served by the other.
    describe_cache: DescribeCache,
}

impl PostgresqlHandlerFactory {
    fn handler(&self) -> PostgresqlHandler {
        PostgresqlHandler {
            connection_id: self.connection_id,
            llm_client: self.llm_client.clone(),
            app_state: self.app_state.clone(),
            status_tx: self.status_tx.clone(),
            server_id: self.server_id,
            remote_addr: self.remote_addr,
            protocol: Arc::new(PostgresqlProtocol::new(
                self.connection_id,
                self.app_state.clone(),
                self.status_tx.clone(),
            )),
            describe_cache: Arc::clone(&self.describe_cache),
        }
    }
}

impl PgWireServerHandlers for PostgresqlHandlerFactory {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        Arc::new(self.handler())
    }

    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        Arc::new(self.handler())
    }

    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        Arc::new(PostgresqlNoopHandler)
    }
}

/// PostgreSQL connection handler
pub struct PostgresqlHandler {
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    #[allow(dead_code)]
    server_id: Option<crate::state::ServerId>,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    /// PostgreSQL protocol handler for action execution
    protocol: Arc<PostgresqlProtocol>,
    /// Describe -> Execute correlation for the extended query protocol
    describe_cache: DescribeCache,
}

/// The outcome of resolving one SQL statement through the LLM/handler pipeline.
///
/// Extended-protocol clients ask for the row description (Describe) *before* asking for the
/// rows (Execute). Because the schema is whatever the LLM decides, the two steps must agree,
/// so the resolved outcome is cached per SQL text between them. This is per-connection
/// protocol bookkeeping, not a data store: nothing is retained once the statement executes.
enum PgOutcome {
    Rows {
        fields: Arc<Vec<FieldInfo>>,
        rows: Vec<PgWireResult<DataRow>>,
    },
    Tag(String),
    Close,
}

/// Cap on described-but-not-executed statements held per connection.
const MAX_PENDING_DESCRIBES: usize = 64;

/// A connection-scoped cache shared by the simple and extended handlers.
type DescribeCache = Arc<TokioMutex<Vec<(String, PgOutcome)>>>;

impl PostgresqlHandler {
    /// Run one statement through the handler pipeline and translate the resulting action into
    /// wire-level output. Returns `Err` for `postgresql_error_response` and for LLM failures.
    async fn resolve(&self, sql: &str) -> PgWireResult<PgOutcome> {
        debug!("PostgreSQL query: {}", sql);
        let _ = self
            .status_tx
            .send(format!("[DEBUG] PostgreSQL query: {}", sql));

        let event = Event::new(
            &POSTGRESQL_QUERY_EVENT,
            serde_json::json!({
                "query": sql,
            }),
        );

        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(0));

        let execution_result = call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        .map_err(|e| {
            // An ErrorResponse carrying a SQLSTATE, not silence: a client that gets nothing
            // back sits in its own read until it times out, and cannot tell an unavailable
            // backend from a slow query.
            //
            // Overload is reported as 53300 (too_many_connections, class 53 "insufficient
            // resources"), which drivers classify as transient; everything else stays XX000
            // (internal_error). The two are deliberately distinguishable — an outage must not
            // look like a permanent fault, and neither may look like success.
            let overloaded = crate::llm::is_overload_error(&e);
            error!(
                "LLM error for PostgreSQL query on connection {} (overload={}): {}",
                self.connection_id, overloaded, e
            );
            let message = crate::utils::WireFailure::classify(&e).prefixed_text();
            let code = if overloaded {
                warn!(
                    "PostgreSQL connection {}: LLM capacity exhausted, replying 53300",
                    self.connection_id
                );
                "53300"
            } else {
                "XX000"
            };
            let _ = self
                .status_tx
                .send(format!("[ERROR] PostgreSQL {}: {}", code, message));
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                code.to_string(),
                message.to_string(),
            )))
        })?;

        let mut close_requested = false;

        for result in execution_result.protocol_results {
            match result {
                ActionResult::CloseConnection => close_requested = true,
                ActionResult::Custom { name, data } => match name.as_str() {
                    "postgresql_query_response" => {
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
                        return build_row_outcome(&columns, &rows);
                    }
                    "postgresql_ok" => {
                        let tag = data.get("tag").and_then(|v| v.as_str()).unwrap_or("OK");
                        return Ok(PgOutcome::Tag(tag.to_string()));
                    }
                    "postgresql_error" => {
                        let severity = data
                            .get("severity")
                            .and_then(|v| v.as_str())
                            .unwrap_or("ERROR")
                            .to_string();
                        let code = data
                            .get("code")
                            .and_then(|v| v.as_str())
                            .unwrap_or("XX000")
                            .to_string();
                        let message = data
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Unknown error")
                            .to_string();

                        let _ = self.status_tx.send(format!(
                            "[ERROR] PostgreSQL error {} {}: {}",
                            severity, code, message
                        ));

                        return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                            severity, code, message,
                        ))));
                    }
                    other => {
                        warn!(
                            "PostgreSQL: no wire encoding for action result '{}', ignoring",
                            other
                        );
                    }
                },
                _ => {}
            }
        }

        if close_requested {
            return Ok(PgOutcome::Close);
        }

        // No response action matched: the handler ran but produced nothing this protocol can
        // encode — a model that refused, a static handler with an empty list, or an answer
        // whose actions were all unrecognised.
        //
        // This used to answer success. A SELECT got an empty result set, which reads as "the
        // query ran and matched no rows" — a factual claim about the data that nothing
        // supports. Worse, anything else got the command tag `OK`, so an INSERT, UPDATE or
        // DELETE the model declined was reported to the client as having completed, and a
        // caller would carry on believing the write landed.
        //
        // Neither is recoverable by the client, because success is indistinguishable from a
        // real one. Fail closed instead, with the same shape the backend-error path above uses
        // so the two are consistent on the wire. 02000 (no_data) is deliberately NOT used: it
        // would again assert something about the data rather than about netget.
        warn!(
            "PostgreSQL: no response action produced for {:?} (decision=fail_closed_no_action)",
            sql
        );
        let _ = self.status_tx.send(
            "[ERROR] PostgreSQL: no response action produced (decision=fail_closed_no_action)"
                .to_string(),
        );
        Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_string(),
            "XX000".to_string(),
            crate::utils::WireFailure::Unavailable
                .prefixed_text()
                .to_string(),
        ))))
    }

    /// Resolve `sql`, storing the outcome so a following Execute reuses it (one LLM call per
    /// extended-protocol statement instead of one per Describe *and* one per Execute).
    async fn resolve_for_describe(&self, sql: &str) -> PgWireResult<Vec<FieldInfo>> {
        let outcome = self.resolve(sql).await?;
        let fields = match &outcome {
            PgOutcome::Rows { fields, .. } => fields.as_ref().clone(),
            _ => Vec::new(),
        };

        let mut cache = self.describe_cache.lock().await;
        cache.retain(|(key, _)| key != sql);
        if cache.len() >= MAX_PENDING_DESCRIBES {
            cache.remove(0);
        }
        cache.push((sql.to_string(), outcome));

        Ok(fields)
    }

    /// Take a previously described outcome, or resolve fresh if Execute arrived without one.
    async fn take_or_resolve(&self, sql: &str) -> PgWireResult<PgOutcome> {
        let cached = {
            let mut cache = self.describe_cache.lock().await;
            cache
                .iter()
                .position(|(key, _)| key == sql)
                .map(|idx| cache.remove(idx).1)
        };

        match cached {
            Some(outcome) => Ok(outcome),
            None => self.resolve(sql).await,
        }
    }
}

/// Error returned when the LLM asked to close the connection.
fn terminating_error() -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "FATAL".to_string(),
        "57P01".to_string(),
        "terminating connection due to administrator command".to_string(),
    )))
}

/// Map a column type name from the LLM onto a PostgreSQL type OID.
fn pg_type_for(type_name: &str) -> Type {
    match type_name.to_lowercase().as_str() {
        "int2" | "smallint" => Type::INT2,
        "int4" | "int" | "integer" => Type::INT4,
        "int8" | "bigint" => Type::INT8,
        "float4" | "real" => Type::FLOAT4,
        "float8" | "double" | "double precision" => Type::FLOAT8,
        "bool" | "boolean" => Type::BOOL,
        "date" => Type::DATE,
        "time" => Type::TIME,
        "timestamp" => Type::TIMESTAMP,
        _ => Type::VARCHAR,
    }
}

/// Encode LLM-supplied columns and rows into a row-description + data-row outcome.
fn build_row_outcome(
    columns: &[serde_json::Value],
    rows: &[serde_json::Value],
) -> PgWireResult<PgOutcome> {
    let fields: Vec<FieldInfo> = columns
        .iter()
        .enumerate()
        .map(|(idx, col)| {
            let name = col
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("column{}", idx + 1));
            let type_name = col.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            FieldInfo::new(name, None, None, pg_type_for(type_name), FieldFormat::Text)
        })
        .collect();

    let fields = Arc::new(fields);
    let mut data_rows = Vec::with_capacity(rows.len());

    for row_data in rows {
        let Some(row_values) = row_data.as_array() else {
            warn!(
                "PostgreSQL: skipping row that is not an array: {}",
                row_data
            );
            continue;
        };

        let mut encoder = DataRowEncoder::new(Arc::clone(&fields));
        // A short row is padded with NULLs and a long row is truncated: PostgreSQL requires
        // exactly one value per described column, and the LLM does occasionally miscount.
        for idx in 0..fields.len() {
            let value = row_values.get(idx).unwrap_or(&serde_json::Value::Null);
            let field_type = fields[idx].datatype();
            encode_value(&mut encoder, field_type, value)?;
        }
        data_rows.push(encoder.finish());
    }

    Ok(PgOutcome::Rows {
        fields,
        rows: data_rows,
    })
}

fn encode_value(
    encoder: &mut DataRowEncoder,
    field_type: &Type,
    value: &serde_json::Value,
) -> PgWireResult<()> {
    if value.is_null() {
        return encoder.encode_field_with_type_and_format(
            &None::<&str>,
            field_type,
            FieldFormat::Text,
            &FormatOptions::default(),
        );
    }

    match *field_type {
        Type::INT2 => encoder.encode_field_with_type_and_format(
            &(value.as_i64().unwrap_or(0) as i16),
            &Type::INT2,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        Type::INT4 => encoder.encode_field_with_type_and_format(
            &(value.as_i64().unwrap_or(0) as i32),
            &Type::INT4,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        Type::INT8 => encoder.encode_field_with_type_and_format(
            &value.as_i64().unwrap_or(0),
            &Type::INT8,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        Type::FLOAT4 => encoder.encode_field_with_type_and_format(
            &(value.as_f64().unwrap_or(0.0) as f32),
            &Type::FLOAT4,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        Type::FLOAT8 => encoder.encode_field_with_type_and_format(
            &value.as_f64().unwrap_or(0.0),
            &Type::FLOAT8,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        Type::BOOL => encoder.encode_field_with_type_and_format(
            &value.as_bool().unwrap_or(false),
            &Type::BOOL,
            FieldFormat::Text,
            &FormatOptions::default(),
        ),
        _ => {
            let value_str = json_value_to_string(value);
            encoder.encode_field_with_type_and_format(
                &value_str.as_str(),
                field_type,
                FieldFormat::Text,
                &FormatOptions::default(),
            )
        }
    }
}

#[async_trait::async_trait]
impl SimpleQueryHandler for PostgresqlHandler {
    async fn do_query<C>(&self, _client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        match self.resolve(query).await? {
            PgOutcome::Rows { fields, rows } => Ok(vec![Response::Query(QueryResponse::new(
                fields,
                futures::stream::iter(rows),
            ))]),
            PgOutcome::Tag(tag) => Ok(vec![Response::Execution(Tag::new(&tag))]),
            PgOutcome::Close => Err(terminating_error()),
        }
    }
}

#[async_trait::async_trait]
impl ExtendedQueryHandler for PostgresqlHandler {
    type Statement = String;
    type QueryParser = PostgresqlQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        Arc::new(PostgresqlQueryParser)
    }

    async fn do_query<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let sql = &portal.statement.statement;

        match self.take_or_resolve(sql).await? {
            PgOutcome::Rows { fields, rows } => Ok(Response::Query(QueryResponse::new(
                fields,
                futures::stream::iter(rows),
            ))),
            PgOutcome::Tag(tag) => Ok(Response::Execution(Tag::new(&tag))),
            PgOutcome::Close => Err(terminating_error()),
        }
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        stmt: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        // The schema is whatever the LLM returns, so it has to be resolved here rather than
        // guessed. Previously this returned an unconditional empty field list, which told
        // every extended-protocol client that the statement produced zero columns and then
        // sent it data rows anyway.
        let fields = self.resolve_for_describe(&stmt.statement).await?;
        Ok(DescribeStatementResponse::new(vec![], fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let fields = self
            .resolve_for_describe(&portal.statement.statement)
            .await?;
        Ok(DescribePortalResponse::new(fields))
    }
}

/// Query parser for PostgreSQL
pub struct PostgresqlQueryParser;

#[async_trait::async_trait]
impl pgwire::api::stmt::QueryParser for PostgresqlQueryParser {
    type Statement = String;

    async fn parse_sql<C>(
        &self,
        _client: &C,
        sql: &str,
        _types: &[Type],
    ) -> PgWireResult<Self::Statement>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        Ok(sql.to_string())
    }
}

/// Convert JSON value to string representation
fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "".to_string(),
        serde_json::Value::Bool(b) => if *b { "t" } else { "f" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
    }
}
