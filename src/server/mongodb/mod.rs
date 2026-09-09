//! MongoDB server implementation with manual OP_MSG parsing
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::{console_debug, console_error};
use actions::{MongodbProtocol, MONGODB_COMMAND_EVENT, MONGODB_DISCONNECTED_EVENT};
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

#[cfg(feature = "mongodb-server")]
use bson::{doc, Bson, Document};

/// Largest MongoDB wire message we will accept.
///
/// Matches the server-advertised `maxMessageSizeBytes` (48 MB). The header's `messageLength`
/// is attacker-controlled, so it has to be range-checked before it is used as an allocation
/// size - see `read_message_body`.
const MAX_MESSAGE_SIZE: i32 = 48 * 1024 * 1024;

/// MongoDB wire protocol opcode for OP_MSG (MongoDB 3.6+).
const OP_MSG: i32 = 2013;

/// How long the rest of a message may take to arrive once its 16-byte header has.
///
/// [`MAX_MESSAGE_SIZE`] bounds one buffer; this bounds how long a peer may hold it. Without
/// it, sixteen bytes claiming a 48 MB body pin 48 MB per connection indefinitely.
const BODY_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// MongoDB server implementation
pub struct MongodbServer {
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    _status_tx: mpsc::UnboundedSender<String>,
    server_id: Option<crate::state::ServerId>,
}

impl MongodbServer {
    /// Create a new MongoDB server
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

    /// Spawn MongoDB server with LLM integration
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener = TcpListener::bind(listen_addr).await?;
        let actual_addr = listener.local_addr()?;

        info!("MongoDB server starting on {}", actual_addr);
        let _ = status_tx.send(format!(
            "[INFO] MongoDB server listening on {}",
            actual_addr
        ));

        let server = Arc::new(MongodbServer::new(
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
                        console_debug!(status_tx, "MongoDB connection from {}", addr);

                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(actual_addr);

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

                        let handler = MongodbHandler::new(
                            connection_id,
                            server.llm_client.clone(),
                            server.app_state.clone(),
                            status_tx.clone(),
                            server.server_id,
                            addr,
                        );

                        tokio::spawn(async move {
                            if let Err(e) = handler.handle_connection(stream).await {
                                error!("MongoDB connection error: {:?}", e);
                            }
                        });
                    }
                    Err(e) => {
                        console_error!(status_tx, "MongoDB accept error: {}", e);
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

/// MongoDB connection handler
pub struct MongodbHandler {
    connection_id: ConnectionId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    #[allow(dead_code)]
    status_tx: mpsc::UnboundedSender<String>,
    #[allow(dead_code)]
    server_id: Option<crate::state::ServerId>,
    #[allow(dead_code)]
    remote_addr: SocketAddr,
    /// MongoDB protocol handler for action execution
    protocol: Arc<MongodbProtocol>,
}

impl MongodbHandler {
    pub fn new(
        connection_id: ConnectionId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: Option<crate::state::ServerId>,
        remote_addr: SocketAddr,
    ) -> Self {
        let protocol = Arc::new(MongodbProtocol::new(
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
        }
    }

    /// Run the connection, then always mark it closed in `AppState`.
    async fn handle_connection(self, stream: TcpStream) -> Result<()> {
        let server_id = self.server_id;
        let connection_id = self.connection_id;
        let app_state = self.app_state.clone();

        let result = self.run(stream).await;

        if let Some(server_id) = server_id {
            app_state
                .close_connection_on_server(server_id, connection_id)
                .await;
        }

        result
    }

    /// Write a reply and count it.
    ///
    /// Every response goes through here so the rail's `↑` counter and `last_activity` cannot
    /// drift from what actually left the socket — MongoDB is connection-oriented, so nothing
    /// else refreshes them.
    async fn write_response<W>(&self, writer: &mut W, bytes: &[u8]) -> Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        writer.write_all(bytes).await?;
        self.record_sent(bytes.len() as u64).await;
        Ok(())
    }

    async fn record_received(&self, bytes: u64) {
        if let Some(server_id) = self.server_id {
            self.app_state
                .update_connection_stats(
                    server_id,
                    self.connection_id,
                    Some(bytes),
                    None,
                    Some(1),
                    None,
                )
                .await;
        }
    }

    async fn record_sent(&self, bytes: u64) {
        if let Some(server_id) = self.server_id {
            self.app_state
                .update_connection_stats(
                    server_id,
                    self.connection_id,
                    None,
                    Some(bytes),
                    None,
                    Some(1),
                )
                .await;
        }
    }

    async fn run(self, mut stream: TcpStream) -> Result<()> {
        debug!(
            "MongoDB handler starting for connection {}",
            self.connection_id
        );

        // MongoDB doesn't require handshake - client sends first
        // The disconnect reason reported to the LLM once the socket is done.
        let mut disconnect_reason = "client_disconnect";

        // `stream` is dropped at the end of this block so the socket is released before the
        // disconnected event goes to the LLM, rather than being held open for the round-trip.
        {
            let (mut reader, mut writer) = stream.split();

            loop {
                // Read MongoDB wire protocol message header (16 bytes)
                // Format: messageLength (4) + requestID (4) + responseTo (4) + opCode (4)
                let mut header = [0u8; 16];
                match reader.read_exact(&mut header).await {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        debug!("MongoDB client disconnected");
                        break;
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }

                let message_length =
                    i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                let request_id = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
                let _response_to =
                    i32::from_le_bytes([header[8], header[9], header[10], header[11]]);
                let op_code = i32::from_le_bytes([header[12], header[13], header[14], header[15]]);

                trace!(
                    "MongoDB message: length={}, requestID={}, opCode={}",
                    message_length,
                    request_id,
                    op_code
                );

                // `messageLength` comes off the wire. A value below 16 used to underflow the
                // `message_length - 16` subtraction into a ~18 exabyte `vec![0u8; n]`, aborting
                // the process; a value near i32::MAX allocated gigabytes per connection. Both are
                // reachable with 16 bytes from an unauthenticated peer.
                if !(16..=MAX_MESSAGE_SIZE).contains(&message_length) {
                    error!(
                        "MongoDB: rejecting message with out-of-range length {} from {}",
                        message_length, self.remote_addr
                    );
                    let _ = self.status_tx.send(format!(
                        "[ERROR] MongoDB: invalid message length {} from {}, closing connection",
                        message_length, self.remote_addr
                    ));
                    disconnect_reason = "invalid_message_length";
                    break;
                }

                // Read the rest of the message body.
                //
                // The length check above bounds one message at 48 MB, but nothing bounded how
                // long the peer could take to deliver it. A client that sends a 48 MB header
                // and then stops holds that whole buffer for the life of the process, and a
                // hundred such connections is 4.8 GB with sixteen bytes sent each. The rest of
                // a message whose header has already arrived is in flight by definition, so a
                // deadline here refuses a stalled peer without truncating a legitimate one.
                let body_length = (message_length - 16) as usize;
                let mut body = vec![0u8; body_length];
                match tokio::time::timeout(BODY_READ_TIMEOUT, reader.read_exact(&mut body)).await {
                    Ok(r) => {
                        r?;
                    }
                    Err(_) => {
                        error!(
                            "MongoDB: {} did not finish a {}-byte message within {:?}, closing",
                            self.remote_addr, message_length, BODY_READ_TIMEOUT
                        );
                        let _ = self.status_tx.send(format!(
                            "[ERROR] MongoDB: incomplete message body from {} after {:?}, \
                             closing connection",
                            self.remote_addr, BODY_READ_TIMEOUT
                        ));
                        disconnect_reason = "incomplete_message_body";
                        break;
                    }
                }
                self.record_received(message_length as u64).await;

                // Parse command based on opCode. Only OP_MSG is implemented; anything else would
                // leave the client waiting forever for a reply it can parse, so close instead.
                if op_code != OP_MSG {
                    error!(
                        "MongoDB: unsupported opCode {} from {}, closing connection",
                        op_code, self.remote_addr
                    );
                    let _ = self.status_tx.send(format!(
                        "[ERROR] MongoDB: unsupported opCode {} (only OP_MSG {} is implemented)",
                        op_code, OP_MSG
                    ));
                    disconnect_reason = "unsupported_opcode";
                    break;
                }

                let command_doc = match self.parse_op_msg(&body) {
                    Ok(doc) => doc,
                    Err(e) => {
                        error!("MongoDB: malformed OP_MSG from {}: {}", self.remote_addr, e);
                        let _ = self
                            .status_tx
                            .send(format!("[ERROR] MongoDB: malformed OP_MSG: {}", e));
                        disconnect_reason = "malformed_op_msg";
                        break;
                    }
                };

                trace!("MongoDB command document: {:?}", command_doc);

                // In the MongoDB command format the *first* key is the command name and its value
                // is the collection, e.g. `{find: "users", filter: {...}, $db: "testdb"}`. The old
                // code looked for a literal "collection" field, which no command ever sends, so
                // the documented `collection` event parameter was always null.
                let command_name = command_doc
                    .keys()
                    .next()
                    .map(|k| k.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let collection = command_doc
                    .get(&command_name)
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let database = command_doc.get_str("$db").unwrap_or("admin").to_string();

                // The driver handshake is protocol business, not a question for the model: it must
                // carry the wire-version range and size limits, and a driver that does not get them
                // aborts before it ever sends a real command. This mirrors how the sibling database
                // protocols handle their handshakes (opensrv-mysql, pgwire's startup handler, and
                // MSSQL's hand-written PRELOGIN/LOGIN).
                if command_name.eq_ignore_ascii_case("hello")
                    || command_name.eq_ignore_ascii_case("ismaster")
                {
                    debug!(
                        "MongoDB handshake ({}) from {}",
                        command_name, self.remote_addr
                    );
                    let response_bytes = self.encode_op_msg_response(
                        request_id,
                        hello_response(self.connection_id.as_u32()),
                    )?;
                    self.write_response(&mut writer, &response_bytes).await?;
                    continue;
                }

                // Call LLM with command event
                let event_data = serde_json::json!({
                    "command": command_name,
                    "database": database,
                    "collection": collection,
                    "filter": self.bson_to_json(command_doc.get("filter")),
                    "document": self.bson_to_json(command_doc.get("documents").or_else(|| command_doc.get("document"))),
                });

                let event = Event::new(&MONGODB_COMMAND_EVENT, event_data);

                let server_id = self
                    .server_id
                    .unwrap_or_else(|| crate::state::ServerId::new(0));
                let execution_result = match call_llm(
                    &self.llm_client,
                    &self.app_state,
                    server_id,
                    Some(self.connection_id),
                    &event,
                    self.protocol.as_ref(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        // `?` here dropped the connection with nothing written, and a
                        // MongoDB driver blocks on the reply to every command it sends, so
                        // the operation hung until the driver's own timeout and was then
                        // reported as a network fault rather than a server error.
                        //
                        // `{ok: 0}` is the only shape a driver reads as a command failure;
                        // anything with `ok: 1` is a result, and an empty result for `find`
                        // means "no documents matched" - a statement about the data.
                        //
                        // The two categories get distinct codes so a client can back off
                        // rather than record a permanent fault. The choice is constrained:
                        // MongoDB's *driver-retryable* codes all describe replica-set
                        // failover (ShutdownInProgress, PrimarySteppedDown,
                        // NotWritablePrimary), and claiming one would send the driver
                        // hunting for a new primary that does not exist. `TemporarilyUnavailable`
                        // (365) says exactly "saturated, try again" without implying a
                        // topology change, so it carries the overload case; everything else
                        // is InternalError (1).
                        let failure = crate::utils::WireFailure::classify(&e);
                        let code = match failure {
                            crate::utils::WireFailure::Overloaded => {
                                MONGODB_TEMPORARILY_UNAVAILABLE
                            }
                            crate::utils::WireFailure::Unavailable => MONGODB_INTERNAL_ERROR,
                        };
                        error!(
                            "MongoDB command '{}' on connection {}: decision=fail_closed_llm_error \
                             category={:?} code={} error={}",
                            command_name, self.connection_id, failure, code, e
                        );
                        let message = failure.prefixed_text();
                        let _ = self.status_tx.send(format!(
                            "[ERROR] MongoDB connection {} decision=fail_closed_llm_error code={}: {}",
                            self.connection_id, code, message
                        ));
                        let doc = mongodb_error_doc(code, message);
                        let response_bytes = self.encode_op_msg_response(request_id, doc)?;
                        self.write_response(&mut writer, &response_bytes).await?;
                        continue;
                    }
                };

                // Execute actions from LLM
                let namespace = format!("{}.{}", database, collection.as_deref().unwrap_or("$cmd"));
                let mut responded = false;
                let mut close_requested = false;

                for protocol_result in execution_result.protocol_results {
                    match protocol_result {
                        ActionResult::Custom { name, data } => {
                            if name == "mongodb_response" {
                                // A model answer the encoder cannot use is a *third* case,
                                // distinct from the backend erroring and from the model
                                // saying nothing. It used to propagate with `?`, which drops
                                // the connection mid-command: the driver blocks on a reply
                                // that never comes and reports a network fault. Answer
                                // `{ok: 0}` and keep the connection, exactly as the LLM-error
                                // branch does.
                                let response_doc = match self.json_to_bson_doc(&data, &namespace) {
                                    Ok(d) => {
                                        if data.get("type").and_then(|v| v.as_str())
                                            == Some("error_response")
                                        {
                                            debug!(
                                                "MongoDB command '{}' on connection {}: \
                                                 decision=model_reject",
                                                command_name, self.connection_id
                                            );
                                        }
                                        d
                                    }
                                    Err(e) => {
                                        error!(
                                            "MongoDB command '{}' on connection {}: \
                                             decision=fail_closed_unusable_answer error={}",
                                            command_name, self.connection_id, e
                                        );
                                        let _ = self.status_tx.send(format!(
                                            "[ERROR] MongoDB connection {} \
                                             decision=fail_closed_unusable_answer for command '{}'",
                                            self.connection_id, command_name
                                        ));
                                        mongodb_error_doc(
                                            MONGODB_INTERNAL_ERROR,
                                            crate::utils::WireFailure::Unavailable.prefixed_text(),
                                        )
                                    }
                                };
                                let response_bytes =
                                    self.encode_op_msg_response(request_id, response_doc)?;
                                self.write_response(&mut writer, &response_bytes).await?;
                                responded = true;
                            } else {
                                warn!(
                                    "MongoDB: no wire encoding for action result '{}', ignoring",
                                    name
                                );
                            }
                        }
                        ActionResult::CloseConnection => {
                            debug!("Closing MongoDB connection");
                            close_requested = true;
                        }
                        ActionResult::NoAction => {}
                        _ => {
                            debug!("Unhandled action result");
                        }
                    }
                }

                if !responded && !close_requested {
                    // MongoDB is strictly request/response: a command with no reply hangs the
                    // driver until its own timeout. Answer with an error instead.
                    warn!(
                        "MongoDB command '{}' on connection {}: decision=fail_closed_no_answer",
                        command_name, self.connection_id
                    );
                    let _ = self.status_tx.send(format!(
                        "[WARN] MongoDB connection {} decision=fail_closed_no_answer for command '{}'",
                        self.connection_id, command_name
                    ));
                    let doc = mongodb_error_doc(
                        59,
                        &format!(
                            "netget: no response produced for command '{}'",
                            command_name
                        ),
                    );
                    let response_bytes = self.encode_op_msg_response(request_id, doc)?;
                    self.write_response(&mut writer, &response_bytes).await?;
                }

                if close_requested {
                    disconnect_reason = "close_this_connection";
                    break;
                }
            }
        }
        drop(stream);

        // Send disconnected event
        let event = Event::new(
            &MONGODB_DISCONNECTED_EVENT,
            serde_json::json!({"reason": disconnect_reason}),
        );
        let server_id = self
            .server_id
            .unwrap_or_else(|| crate::state::ServerId::new(0));
        let _ = call_llm(
            &self.llm_client,
            &self.app_state,
            server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await;

        Ok(())
    }

    /// Parse OP_MSG body (MongoDB 3.6+ wire protocol)
    #[cfg(feature = "mongodb-server")]
    fn parse_op_msg(&self, body: &[u8]) -> Result<Document> {
        // OP_MSG format: flagBits (4) + sections
        // We only handle section kind 0 (body document)
        if body.len() < 5 {
            return Err(anyhow::anyhow!("OP_MSG body too short"));
        }

        let _flag_bits = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
        let section_kind = body[4];

        if section_kind != 0 {
            return Err(anyhow::anyhow!(
                "Unsupported OP_MSG section kind: {}",
                section_kind
            ));
        }

        // Parse BSON document starting at byte 5
        let doc = Document::from_reader(&body[5..])?;
        Ok(doc)
    }

    #[cfg(not(feature = "mongodb-server"))]
    fn parse_op_msg(&self, _body: &[u8]) -> Result<Document> {
        Err(anyhow::anyhow!("MongoDB server feature not enabled"))
    }

    /// Encode OP_MSG response
    #[cfg(feature = "mongodb-server")]
    fn encode_op_msg_response(&self, request_id: i32, doc: Document) -> Result<Vec<u8>> {
        let mut body = vec![0u8; 5]; // flagBits (4) + section kind (1)
        body[4] = 0; // Section kind 0 (body)

        // Serialize BSON document
        let mut doc_bytes = Vec::new();
        doc.to_writer(&mut doc_bytes)?;
        body.extend_from_slice(&doc_bytes);

        // Create header
        let message_length = (16 + body.len()) as i32;
        let response_to = request_id;
        let op_code = 2013i32; // OP_MSG

        let mut message = Vec::new();
        message.extend_from_slice(&message_length.to_le_bytes());
        message.extend_from_slice(&0i32.to_le_bytes()); // responseID (0 = server)
        message.extend_from_slice(&response_to.to_le_bytes());
        message.extend_from_slice(&op_code.to_le_bytes());
        message.extend_from_slice(&body);

        Ok(message)
    }

    #[cfg(not(feature = "mongodb-server"))]
    fn encode_op_msg_response(&self, _request_id: i32, _doc: Document) -> Result<Vec<u8>> {
        Err(anyhow::anyhow!("MongoDB server feature not enabled"))
    }

    /// Convert BSON to JSON
    #[cfg(feature = "mongodb-server")]
    fn bson_to_json(&self, bson_opt: Option<&Bson>) -> serde_json::Value {
        match bson_opt {
            Some(bson) => bson.clone().into_relaxed_extjson(),
            None => serde_json::Value::Null,
        }
    }

    #[cfg(not(feature = "mongodb-server"))]
    fn bson_to_json(&self, _bson_opt: Option<&Bson>) -> serde_json::Value {
        serde_json::Value::Null
    }

    /// Convert JSON action to BSON document for response
    #[cfg(feature = "mongodb-server")]
    fn json_to_bson_doc(&self, json: &serde_json::Value, namespace: &str) -> Result<Document> {
        let action_type = json
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing action type")?;

        match action_type {
            "find_response" => {
                let documents = json
                    .get("documents")
                    .and_then(|v| v.as_array())
                    .context("Missing documents")?;

                let cursor_docs: Vec<Bson> = documents
                    .iter()
                    .filter_map(|d| d.clone().try_into().ok())
                    .collect();

                Ok(doc! {
                    "ok": 1,
                    "cursor": {
                        "id": 0i64,
                        // The namespace must name the collection the client actually queried;
                        // this used to be hardcoded to "test.collection".
                        "ns": namespace,
                        "firstBatch": cursor_docs
                    }
                })
            }
            "insert_response" => {
                let n = json
                    .get("inserted_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1) as i32;
                Ok(doc! { "ok": 1, "n": n })
            }
            "update_response" => {
                let matched = json
                    .get("matched_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as i32;
                let modified = json
                    .get("modified_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as i32;
                Ok(doc! { "ok": 1, "n": matched, "nModified": modified })
            }
            "delete_response" => {
                let n = json
                    .get("deleted_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as i32;
                Ok(doc! { "ok": 1, "n": n })
            }
            "error_response" => {
                let code = json.get("code").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let message = json
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown error");
                Ok(mongodb_error_doc(code, message))
            }
            other => Err(anyhow::anyhow!(
                "MongoDB: unknown response action type '{}'",
                other
            )),
        }
    }

    #[cfg(not(feature = "mongodb-server"))]
    fn json_to_bson_doc(&self, _json: &serde_json::Value, _namespace: &str) -> Result<Document> {
        Err(anyhow::anyhow!("MongoDB server feature not enabled"))
    }
}

/// MongoDB `InternalError`: "an internal server error occurred". Paired with `ok: 0`, which is
/// what makes a driver raise rather than return a result.
#[cfg(feature = "mongodb-server")]
const MONGODB_INTERNAL_ERROR: i32 = 1;

/// MongoDB `TemporarilyUnavailable` (6.0+): the server is saturated and the same request may
/// succeed later. Used for [`crate::utils::WireFailure::Overloaded`] so a client backs off
/// instead of recording a permanent fault. Unlike the driver-retryable codes it does not
/// assert anything about replica-set topology.
#[cfg(feature = "mongodb-server")]
const MONGODB_TEMPORARILY_UNAVAILABLE: i32 = 365;

/// Build a MongoDB command-failure document.
#[cfg(feature = "mongodb-server")]
fn mongodb_error_doc(code: i32, message: &str) -> Document {
    doc! { "ok": 0, "code": code, "errmsg": message }
}

/// The reply to `hello` / `isMaster`.
///
/// A MongoDB driver refuses to use a server that does not advertise a wire-version range it
/// supports, so these fields cannot be left to the model. Wire version 17 is MongoDB 6.0.
#[cfg(feature = "mongodb-server")]
fn hello_response(connection_id: u32) -> Document {
    doc! {
        "ok": 1,
        "isWritablePrimary": true,
        "ismaster": true,
        "helloOk": true,
        "readOnly": false,
        "minWireVersion": 0i32,
        "maxWireVersion": 17i32,
        "maxBsonObjectSize": 16 * 1024 * 1024i32,
        "maxMessageSizeBytes": MAX_MESSAGE_SIZE,
        "maxWriteBatchSize": 100_000i32,
        "logicalSessionTimeoutMinutes": 30i32,
        "connectionId": connection_id as i32,
        "localTime": bson::DateTime::now(),
    }
}
