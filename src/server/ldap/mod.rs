//! LDAPv3 server with hand-written ASN.1 BER coding.
//!
//! No LDAP crate is used, so this module decodes attacker-controlled BER by hand — the
//! highest-risk parsing in the file-service protocol group. The invariant every decoder here
//! keeps: **no index or range is used before it has been checked against the remaining
//! buffer.** `read_ber_element` is the single entry point, and it validates that an element's
//! claimed length actually fits before handing back a `value` slice; nothing else slices raw
//! input. Recursion (search filters) is depth-limited, and message size is capped.
//!
//! There is no directory. A bind is granted by the model, a search returns entries the model
//! names, and add/modify/delete are acknowledged without anything changing here, because there
//! is nothing here to change. Whether a following search agrees with an earlier add is the
//! model's memory to keep.
//!
//! Not implemented: SASL (the mechanism is reported so it can be refused), StartTLS, LDAPS,
//! referrals, controls, extended operations, compare, modifyDN, and abandon. Search scope,
//! filter and requested-attribute list are decoded and handed to the model as text, but never
//! evaluated here.
pub mod actions;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

#[cfg(feature = "ldap")]
use crate::llm::action_helper::call_llm;
#[cfg(feature = "ldap")]
use crate::llm::ollama_client::OllamaClient;
#[cfg(feature = "ldap")]
use crate::llm::ActionResult;
#[cfg(feature = "ldap")]
use crate::logging::emit::Log;
#[cfg(feature = "ldap")]
use crate::protocol::Event;
#[cfg(feature = "ldap")]
use crate::server::LdapProtocol;
#[cfg(feature = "ldap")]
use crate::state::app_state::AppState;
#[cfg(feature = "ldap")]
use actions::{
    LDAP_ADD_EVENT, LDAP_BIND_EVENT, LDAP_DELETE_EVENT, LDAP_MODIFY_EVENT, LDAP_SEARCH_EVENT,
    LDAP_UNBIND_EVENT,
};

/// LDAP server that handles directory operations with LLM
pub struct LdapServer;

#[cfg(feature = "ldap")]
impl LdapServer {
    /// Spawn LDAP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        Log::new(Some(&status_tx)).info(format!(
            "LDAP server (action-based) listening on {}",
            local_addr
        ));

        let protocol = Arc::new(LdapProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id = crate::server::connection::ConnectionId::new(
                            app_state.get_next_unified_id().await,
                        );
                        Log::new(Some(&status_tx)).info(format!(
                            "LDAP connection {} from {}",
                            connection_id, remote_addr
                        ));

                        // Register the peer in AppState. Without this the dashboard rail
                        // showed an LDAP server with no peers at all while clients were bound
                        // to it, `[ message this peer ]` had nothing to act on, and no
                        // connection-scoped scheduled task could ever be created. LDAP is
                        // connection-oriented, so it must not declare `.connectionless()` and
                        // the 10-second idle sweep deliberately does not touch these entries -
                        // the close below is the only thing that retires one.
                        {
                            use crate::state::server::{
                                ConnectionState as ServerConnectionState, ConnectionStatus,
                                ProtocolConnectionInfo,
                            };
                            let now = std::time::Instant::now();
                            let local_addr = stream.local_addr().unwrap_or(local_addr);
                            app_state
                                .add_connection_to_server(
                                    server_id,
                                    ServerConnectionState {
                                        id: connection_id,
                                        remote_addr,
                                        local_addr,
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
                            let _ = status_tx.send("__UPDATE_UI__".to_string());
                        }

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let mut session = LdapSession {
                                stream,
                                connection_id,
                                server_id,
                                llm_client: llm_clone.clone(),
                                app_state: state_clone.clone(),
                                status_tx: status_clone.clone(),
                                protocol: protocol_clone.clone(),
                                authenticated: false,
                                bind_dn: None,
                            };

                            // Handle LDAP session
                            if let Err(e) = session.handle().await {
                                Log::new(Some(&status_clone))
                                    .error(format!("LDAP session error: {}", e));
                            }

                            state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            let _ = status_clone.send("__UPDATE_UI__".to_string());

                            Log::new(Some(&status_clone))
                                .info(format!("LDAP connection {} closed", connection_id));
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept LDAP connection: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

#[cfg(feature = "ldap")]
struct LdapSession {
    stream: tokio::net::TcpStream,
    connection_id: crate::server::connection::ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<LdapProtocol>,
    authenticated: bool,
    bind_dn: Option<String>,
}

#[cfg(feature = "ldap")]
impl LdapSession {
    /// Read LDAP messages off the socket and answer them.
    ///
    /// Messages are framed by their BER length rather than by read boundaries. The previous
    /// implementation treated one `read()` as exactly one message, which is wrong in both
    /// directions: `ldapsearch` pipelines bind and search into a single segment (the second
    /// message was silently discarded), and any message larger than the 8 KiB buffer, or split
    /// across segments by the network, was rejected as malformed.
    async fn handle(&mut self) -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; READ_CHUNK];

        loop {
            // Drain every complete message already buffered before reading more.
            loop {
                let message_len = match ldap_message_len(&buffer) {
                    Ok(Some(len)) => len,
                    Ok(None) => break, // need more bytes
                    Err(e) => {
                        Log::new(Some(&self.status_tx)).error(format!("LDAP framing error: {}", e));
                        return Ok(());
                    }
                };

                let message: Vec<u8> = buffer.drain(..message_len).collect();
                trace!("LDAP message {} bytes: {:02x?}", message.len(), message);

                let step = match self.handle_message(&message).await {
                    Ok(step) => step,
                    Err(e) => {
                        Log::new(Some(&self.status_tx)).error(format!("LDAP parse error: {}", e));
                        return Ok(());
                    }
                };

                let (response, close_after) = match step {
                    SessionStep::Respond(response) => (Some(response), false),
                    SessionStep::RespondAndClose(response) => (Some(response), true),
                    SessionStep::Close => (None, true),
                };

                if let Some(response) = response {
                    Log::new(Some(&self.status_tx)).trace(format!(
                        "LDAP sending {} bytes: {:02x?}",
                        response.len(),
                        response
                    ));
                    self.stream.write_all(&response).await?;
                    self.stream.flush().await?;
                    self.record_bytes(0, response.len() as u64).await;
                }

                if close_after {
                    return Ok(());
                }
            }

            if buffer.len() >= MAX_LDAP_MESSAGE {
                // Only reachable when a peer sends a long-form length header promising a huge
                // message; ldap_message_len rejects those, so this is belt and braces.
                error!("LDAP receive buffer exceeded {} bytes", MAX_LDAP_MESSAGE);
                return Ok(());
            }

            let n = match self.stream.read(&mut chunk).await {
                Ok(0) => break, // Connection closed
                Ok(n) => n,
                Err(e) => {
                    Log::new(Some(&self.status_tx)).error(format!("LDAP read error: {}", e));
                    break;
                }
            };
            Log::new(Some(&self.status_tx)).trace(format!("LDAP received {} bytes", n));
            self.record_bytes(n as u64, 0).await;
            buffer.extend_from_slice(&chunk[..n]);
        }

        Ok(())
    }

    /// Keep the connection's byte counters and `last_activity` current.
    ///
    /// The rail's down/up figures and every connection-scoped scheduled-task prompt read
    /// these, and a connection-oriented server has to maintain them itself: the 10-second
    /// idle sweep that would otherwise touch `last_activity` runs only over protocols that
    /// declare `.connectionless()`, which LDAP is not.
    async fn record_bytes(&self, received: u64, sent: u64) {
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                (received > 0).then_some(received),
                (sent > 0).then_some(sent),
                (received > 0).then_some(1),
                (sent > 0).then_some(1),
            )
            .await;
    }

    /// Decode one complete LDAPMessage and dispatch on its protocolOp.
    async fn handle_message(&mut self, data: &[u8]) -> Result<SessionStep> {
        let envelope = read_ber_element(data).context("LDAPMessage envelope")?;
        if envelope.tag != TAG_SEQUENCE {
            anyhow::bail!(
                "Invalid LDAP message: expected SEQUENCE, got tag 0x{:02x}",
                envelope.tag
            );
        }
        let body = envelope.value;

        let id_element = read_ber_element(body).context("messageID")?;
        if id_element.tag != TAG_INTEGER {
            anyhow::bail!(
                "Invalid LDAP message: expected messageID INTEGER, got tag 0x{:02x}",
                id_element.tag
            );
        }
        let msg_id = ber_integer(id_element.value).context("messageID value")? as i32;

        let op = read_ber_element(&body[id_element.total_len..]).context("protocolOp")?;
        debug!(
            "LDAP message id={} op=0x{:02x} ({} bytes)",
            msg_id,
            op.tag,
            op.value.len()
        );

        match op.tag {
            OP_BIND_REQUEST => self.handle_bind_request(msg_id, op.value).await,
            OP_SEARCH_REQUEST => self.handle_search_request(msg_id, op.value).await,
            OP_ADD_REQUEST => self.handle_add_request(msg_id, op.value).await,
            OP_MODIFY_REQUEST => self.handle_modify_request(msg_id, op.value).await,
            OP_DELETE_REQUEST => self.handle_delete_request(msg_id, op.value).await,
            OP_UNBIND_REQUEST => self.handle_unbind_request().await,
            other => {
                Log::new(Some(&self.status_tx))
                    .debug(format!("LDAP unsupported operation: 0x{:02x}", other));
                // Answer with the response type that matches the request where we know it, so
                // the client's decoder does not reject the reply outright. This used to always
                // send a BindResponse, which an ldap3 client answering a search reports as a
                // protocol violation rather than as the protocolError it is.
                Ok(SessionStep::Respond(encode_ldap_result(
                    msg_id,
                    response_tag_for(other),
                    RESULT_PROTOCOL_ERROR,
                    "Operation not supported by this server",
                )))
            }
        }
    }

    /// Run one LLM round-trip for `event` and turn the result into a session step.
    ///
    /// `default` is what the peer gets when the model returns nothing usable - never silence,
    /// because a client with no response simply hangs until its own timeout.
    ///
    /// A model *failure* is answered differently, with `unavailable` (52) or `busy` (51) under
    /// `response_tag`. The distinction matters: the per-operation defaults describe an outcome
    /// the directory chose (invalidCredentials for a bind, an empty result set for a search),
    /// and returning one of those for a backend outage would misreport an outage as a decision.
    /// `unavailable` is the RFC 4511 code for "this server is not able to answer right now",
    /// and unlike the search default it is never a success, so a failure can never be mistaken
    /// for an empty-but-valid answer.
    async fn respond_via_llm(
        &mut self,
        event: crate::protocol::Event,
        default: Vec<u8>,
        msg_id: i32,
        response_tag: u8,
    ) -> (SessionStep, Decided) {
        let execution_result = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            &event,
            self.protocol.as_ref(),
        )
        .await
        {
            Ok(result) => result,
            Err(e) => {
                Log::new(Some(&self.status_tx)).warn(format!(
                    "LDAP LLM call failed on connection {} (msg_id {}): {}",
                    self.connection_id, msg_id, e
                ));

                // busy (51) says "retry shortly", unavailable (52) says "I am down".
                let (code, diagnostic) = if crate::llm::is_overload_error(&e) {
                    warn!(
                        "LDAP busy on connection {}: LLM capacity exhausted",
                        self.connection_id
                    );
                    (RESULT_BUSY, "Server busy, retry later")
                } else {
                    (RESULT_UNAVAILABLE, "Server unavailable")
                };
                // The `decision=` token is the only thing that separates the three
                // outcomes for an operator, because two of them can reach the wire as the
                // same result code. Keep them distinct the way `src/server/radius/` does:
                // conflating "the model denied" with "the model never answered" is the
                // OAuth2 failure mode, and on a directory server that decides who is who it
                // is the difference between a policy denial and an unnoticed outage.
                Log::new(Some(&self.status_tx)).error(format!(
                    "LDAP msg_id {} on connection {}: decision=fail_closed_llm_error \
                     result={} ({})",
                    msg_id, self.connection_id, code, diagnostic
                ));
                return (
                    SessionStep::Respond(encode_ldap_result(
                        msg_id,
                        response_tag,
                        code,
                        diagnostic,
                    )),
                    Decided::ByServer,
                );
            }
        };

        let mut output: Option<Vec<u8>> = None;
        let mut close = false;
        for protocol_result in execution_result.protocol_results {
            match protocol_result {
                ActionResult::Output(data) => {
                    if output.is_none() {
                        output = Some(data);
                    }
                }
                ActionResult::CloseConnection => close = true,
                _ => {}
            }
        }

        match (output, close) {
            (Some(data), false) => (SessionStep::Respond(data), Decided::ByModel),
            (Some(data), true) => (SessionStep::RespondAndClose(data), Decided::ByModel),
            (None, true) => (SessionStep::Close, Decided::ByModel),
            (None, false) => {
                // Silence is not consent. Every per-operation default is a refusal
                // (invalidCredentials for a bind, unwillingToPerform for a write); the one
                // exception is search, whose default is an empty-but-successful result set,
                // and an empty result set discloses nothing. Either way this is the
                // *server's* decision, never the model's, and the token says so.
                Log::new(Some(&self.status_tx)).error(format!(
                    "LDAP msg_id {} on connection {}: decision=fail_closed_no_action \
                     (handler returned no response action; sending the refusal default)",
                    msg_id, self.connection_id
                ));
                (SessionStep::Respond(default), Decided::ByServer)
            }
        }
    }

    async fn handle_bind_request(&mut self, msg_id: i32, data: &[u8]) -> Result<SessionStep> {
        // BindRequest ::= [APPLICATION 0] SEQUENCE {
        //     version INTEGER, name LDAPDN, authentication AuthenticationChoice }
        let version_element = read_ber_element(data).context("bind version")?;
        let version = ber_integer(version_element.value).context("bind version value")?;

        let after_version = &data[version_element.total_len..];
        let name_element = read_ber_element(after_version).context("bind name")?;
        let dn = ber_string(name_element.value);

        let after_name = &after_version[name_element.total_len..];
        let (auth_type, password) = if after_name.is_empty() {
            ("none".to_string(), String::new())
        } else {
            let auth = read_ber_element(after_name).context("bind authentication")?;
            match auth.tag {
                AUTH_SIMPLE => ("simple".to_string(), ber_string(auth.value)),
                AUTH_SASL => {
                    // SaslCredentials ::= SEQUENCE { mechanism LDAPString, credentials OPTIONAL }
                    let mechanism = read_ber_element(auth.value)
                        .map(|m| ber_string(m.value))
                        .unwrap_or_default();
                    (format!("sasl:{}", mechanism), String::new())
                }
                other => (format!("unknown:0x{:02x}", other), String::new()),
            }
        };

        Log::new(Some(&self.status_tx)).debug(format!(
            "LDAP Bind request: version={}, dn={}, auth={}, password_len={}",
            version,
            dn,
            auth_type,
            password.len()
        ));

        let event = Event::new(
            &LDAP_BIND_EVENT,
            serde_json::json!({
                "message_id": msg_id,
                "version": version,
                "dn": dn,
                "password": password,
                "auth_type": auth_type,
            }),
        );

        // Read the bind outcome from the action the model produced rather than by scanning the
        // encoded bytes back out of the response, which is what this used to do.
        let default =
            encode_bind_response(msg_id, RESULT_INVALID_CREDENTIALS, "Invalid credentials");
        let (step, decided) = self
            .respond_via_llm(event, default, msg_id, RESPONSE_BIND)
            .await;

        // `Decided::ByServer` has already logged its own fail_closed_* token, and its
        // response is a refusal by construction. Only a response the model itself produced
        // may be reported as the model's decision - reading success back out of the bytes
        // alone cannot tell the two apart, which is precisely the conflation that turned an
        // LLM outage into an approval in OAuth2.
        if decided == Decided::ByModel {
            if let SessionStep::Respond(ref response) | SessionStep::RespondAndClose(ref response) =
                step
            {
                if bind_succeeded(response) {
                    self.authenticated = true;
                    self.bind_dn = Some(dn.clone());
                    Log::new(Some(&self.status_tx)).info(format!(
                        "LDAP bind for {:?} on connection {}: decision=model_accept",
                        dn, self.connection_id
                    ));
                } else {
                    Log::new(Some(&self.status_tx)).info(format!(
                        "LDAP bind for {:?} on connection {}: decision=model_reject",
                        dn, self.connection_id
                    ));
                }
            }
        }

        Ok(step)
    }

    async fn handle_search_request(&mut self, msg_id: i32, data: &[u8]) -> Result<SessionStep> {
        // SearchRequest ::= [APPLICATION 3] SEQUENCE {
        //     baseObject LDAPDN, scope ENUMERATED, derefAliases ENUMERATED,
        //     sizeLimit INTEGER, timeLimit INTEGER, typesOnly BOOLEAN,
        //     filter Filter, attributes AttributeSelection }
        let base_element = read_ber_element(data).context("search baseObject")?;
        let base_dn = ber_string(base_element.value);

        let mut rest = &data[base_element.total_len..];
        let mut scope = "sub".to_string();
        if let Ok(scope_element) = read_ber_element(rest) {
            scope = match ber_integer(scope_element.value).unwrap_or(2) {
                0 => "base",
                1 => "one",
                _ => "sub",
            }
            .to_string();
            rest = &rest[scope_element.total_len..];
        }

        // derefAliases, sizeLimit, timeLimit, typesOnly: read past them without interpreting.
        for _ in 0..4 {
            match read_ber_element(rest) {
                Ok(element) => rest = &rest[element.total_len..],
                Err(_) => break,
            }
        }

        let filter = match read_ber_element(rest) {
            Ok(filter_element) => {
                let text = render_filter(&filter_element, 0);
                rest = &rest[filter_element.total_len..];
                text
            }
            Err(_) => "(objectClass=*)".to_string(),
        };

        let attributes = match read_ber_element(rest) {
            Ok(list) => {
                let mut names = Vec::new();
                let mut cursor = list.value;
                while let Ok(element) = read_ber_element(cursor) {
                    names.push(serde_json::Value::String(ber_string(element.value)));
                    cursor = &cursor[element.total_len..];
                }
                names
            }
            Err(_) => Vec::new(),
        };

        Log::new(Some(&self.status_tx)).debug(format!(
            "LDAP Search request: base_dn={}, scope={}, filter={}",
            base_dn, scope, filter
        ));

        let event = Event::new(
            &LDAP_SEARCH_EVENT,
            serde_json::json!({
                "message_id": msg_id,
                "base_dn": base_dn,
                "authenticated": self.authenticated,
                "bind_dn": self.bind_dn.as_deref().unwrap_or(""),
                "scope": scope,
                "filter": filter,
                "attributes": attributes,
            }),
        );

        // Default is an empty but successful result set: a client that gets nothing back at
        // all hangs, and a failure code here would look like a directory error rather than an
        // empty search.
        let default = encode_search_done(msg_id, RESULT_SUCCESS, "");
        Ok(self
            .respond_via_llm(event, default, msg_id, RESPONSE_SEARCH_DONE)
            .await
            .0)
    }

    async fn handle_add_request(&mut self, msg_id: i32, data: &[u8]) -> Result<SessionStep> {
        // AddRequest ::= [APPLICATION 8] SEQUENCE {
        //     entry LDAPDN, attributes AttributeList }
        let entry_element = read_ber_element(data).context("add entry DN")?;
        let dn = ber_string(entry_element.value);

        let attributes = match read_ber_element(&data[entry_element.total_len..]) {
            Ok(list) => parse_attribute_list(list.value),
            Err(_) => serde_json::Map::new(),
        };

        Log::new(Some(&self.status_tx)).debug(format!(
            "LDAP Add request: dn={}, {} attributes",
            dn,
            attributes.len()
        ));

        let event = Event::new(
            &LDAP_ADD_EVENT,
            serde_json::json!({
                "message_id": msg_id,
                "dn": dn,
                "attributes": attributes,
                "authenticated": self.authenticated,
                "bind_dn": self.bind_dn.as_deref().unwrap_or(""),
            }),
        );

        let default = encode_ldap_result(
            msg_id,
            RESPONSE_ADD,
            RESULT_UNWILLING_TO_PERFORM,
            "No response from server policy",
        );
        Ok(self
            .respond_via_llm(event, default, msg_id, RESPONSE_ADD)
            .await
            .0)
    }

    async fn handle_modify_request(&mut self, msg_id: i32, data: &[u8]) -> Result<SessionStep> {
        // ModifyRequest ::= [APPLICATION 6] SEQUENCE {
        //     object LDAPDN,
        //     changes SEQUENCE OF SEQUENCE { operation ENUMERATED, modification PartialAttribute } }
        let object_element = read_ber_element(data).context("modify object DN")?;
        let dn = ber_string(object_element.value);

        let mut changes = Vec::new();
        if let Ok(change_list) = read_ber_element(&data[object_element.total_len..]) {
            let mut cursor = change_list.value;
            while let Ok(change) = read_ber_element(cursor) {
                cursor = &cursor[change.total_len..];

                let Ok(operation_element) = read_ber_element(change.value) else {
                    continue;
                };
                let operation = match ber_integer(operation_element.value).unwrap_or(-1) {
                    0 => "add",
                    1 => "delete",
                    2 => "replace",
                    3 => "increment",
                    _ => "unknown",
                };

                let modification = &change.value[operation_element.total_len..];
                let (attribute, values) = match read_ber_element(modification) {
                    Ok(partial) => parse_partial_attribute(partial.value),
                    Err(_) => (String::new(), Vec::new()),
                };

                changes.push(serde_json::json!({
                    "operation": operation,
                    "attribute": attribute,
                    "values": values,
                }));
            }
        }

        Log::new(Some(&self.status_tx)).debug(format!(
            "LDAP Modify request: dn={}, {} changes",
            dn,
            changes.len()
        ));

        let event = Event::new(
            &LDAP_MODIFY_EVENT,
            serde_json::json!({
                "message_id": msg_id,
                "dn": dn,
                "changes": changes,
                "authenticated": self.authenticated,
                "bind_dn": self.bind_dn.as_deref().unwrap_or(""),
            }),
        );

        let default = encode_ldap_result(
            msg_id,
            RESPONSE_MODIFY,
            RESULT_UNWILLING_TO_PERFORM,
            "No response from server policy",
        );
        Ok(self
            .respond_via_llm(event, default, msg_id, RESPONSE_MODIFY)
            .await
            .0)
    }

    async fn handle_delete_request(&mut self, msg_id: i32, data: &[u8]) -> Result<SessionStep> {
        // DelRequest ::= [APPLICATION 10] LDAPDN - the DN is the primitive value itself.
        let dn = ber_string(data);

        Log::new(Some(&self.status_tx)).debug(format!("LDAP Delete request: dn={}", dn));

        let event = Event::new(
            &LDAP_DELETE_EVENT,
            serde_json::json!({
                "message_id": msg_id,
                "dn": dn,
                "authenticated": self.authenticated,
                "bind_dn": self.bind_dn.as_deref().unwrap_or(""),
            }),
        );

        let default = encode_ldap_result(
            msg_id,
            RESPONSE_DELETE,
            RESULT_UNWILLING_TO_PERFORM,
            "No response from server policy",
        );
        Ok(self
            .respond_via_llm(event, default, msg_id, RESPONSE_DELETE)
            .await
            .0)
    }

    async fn handle_unbind_request(&mut self) -> Result<SessionStep> {
        Log::new(Some(&self.status_tx))
            .debug(format!("LDAP Unbind request from {}", self.connection_id));

        // Informational. RFC 4511 forbids a response to an unbind, and the event type says so
        // with .with_no_actions(), so nothing here reads the result - it exists to let script
        // and static handlers observe the disconnect.
        //
        // Raised on a detached task, because awaiting it here delays closing the socket by a
        // whole model round-trip. Verified with ldapadd against a real model: the add was
        // answered promptly and the client then sat for fifteen seconds on unbind waiting for
        // a connection teardown that was blocked on an LLM call whose result is discarded.
        let event = Event::new(
            &LDAP_UNBIND_EVENT,
            serde_json::json!({
                "bind_dn": self.bind_dn.as_deref().unwrap_or(""),
            }),
        );

        let llm_client = self.llm_client.clone();
        let app_state = self.app_state.clone();
        let protocol = self.protocol.clone();
        let server_id = self.server_id;
        let connection_id = self.connection_id;
        tokio::spawn(async move {
            let _ = call_llm(
                &llm_client,
                &app_state,
                server_id,
                Some(connection_id),
                &event,
                protocol.as_ref(),
            )
            .await;
        });

        Ok(SessionStep::Close)
    }
}

/// What the session should do after handling one message.
#[cfg(feature = "ldap")]
/// Who chose the response the peer is about to receive.
///
/// The wire cannot always carry the distinction - a refusal the model chose and a refusal the
/// server substituted can be the same LDAP result code - so it has to survive in the type and
/// reach the log. See the `decision=` tokens in `respond_via_llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decided {
    /// The model produced the response.
    ByModel,
    /// The model produced nothing usable, or the LLM call failed; the server refused on its
    /// own and has already logged a `decision=fail_closed_*` token.
    ByServer,
}

enum SessionStep {
    Respond(Vec<u8>),
    RespondAndClose(Vec<u8>),
    Close,
}

// ============================================================================
// BER decoding
//
// Every function here takes attacker-controlled bytes. The rule is that no index or range is
// used before it has been checked against the remaining buffer: the previous implementation
// indexed `data[op_start]` and sliced `data[start..start + len]` using lengths read straight
// off the wire, so a seven-byte message could panic the connection task - silently, while the
// server kept reporting Running.
// ============================================================================

/// Largest LDAP message accepted, in bytes. A long-form length header can promise up to 4 GiB.
#[cfg(feature = "ldap")]
const MAX_LDAP_MESSAGE: usize = 1 << 20;

/// Socket read size. Unrelated to message size now that messages are reassembled.
#[cfg(feature = "ldap")]
const READ_CHUNK: usize = 8192;

/// Maximum search-filter nesting rendered. Filters are recursive and client-supplied, so an
/// unbounded renderer would overflow the stack on a deliberately deep filter.
#[cfg(feature = "ldap")]
const MAX_FILTER_DEPTH: usize = 32;

#[cfg(feature = "ldap")]
const TAG_SEQUENCE: u8 = 0x30;
#[cfg(feature = "ldap")]
const TAG_INTEGER: u8 = 0x02;

#[cfg(feature = "ldap")]
const OP_BIND_REQUEST: u8 = 0x60;
#[cfg(feature = "ldap")]
const OP_SEARCH_REQUEST: u8 = 0x63;
#[cfg(feature = "ldap")]
const OP_MODIFY_REQUEST: u8 = 0x66;
#[cfg(feature = "ldap")]
const OP_ADD_REQUEST: u8 = 0x68;
#[cfg(feature = "ldap")]
const OP_DELETE_REQUEST: u8 = 0x4A;
#[cfg(feature = "ldap")]
const OP_UNBIND_REQUEST: u8 = 0x42;

#[cfg(feature = "ldap")]
const RESPONSE_BIND: u8 = 0x61;
#[cfg(feature = "ldap")]
const RESPONSE_SEARCH_DONE: u8 = 0x65;
#[cfg(feature = "ldap")]
const RESPONSE_MODIFY: u8 = 0x67;
#[cfg(feature = "ldap")]
const RESPONSE_ADD: u8 = 0x69;
#[cfg(feature = "ldap")]
const RESPONSE_DELETE: u8 = 0x6B;
#[cfg(feature = "ldap")]
const RESPONSE_EXTENDED: u8 = 0x78;

#[cfg(feature = "ldap")]
const AUTH_SIMPLE: u8 = 0x80;
#[cfg(feature = "ldap")]
const AUTH_SASL: u8 = 0xA3;

#[cfg(feature = "ldap")]
const RESULT_SUCCESS: u8 = 0;
#[cfg(feature = "ldap")]
const RESULT_PROTOCOL_ERROR: u8 = 2;
#[cfg(feature = "ldap")]
const RESULT_INVALID_CREDENTIALS: u8 = 49;
#[cfg(feature = "ldap")]
const RESULT_BUSY: u8 = 51;
#[cfg(feature = "ldap")]
const RESULT_UNAVAILABLE: u8 = 52;
#[cfg(feature = "ldap")]
const RESULT_UNWILLING_TO_PERFORM: u8 = 53;

/// The response type matching a request type, for error replies.
#[cfg(feature = "ldap")]
fn response_tag_for(request_tag: u8) -> u8 {
    match request_tag {
        OP_BIND_REQUEST => RESPONSE_BIND,
        OP_SEARCH_REQUEST => RESPONSE_SEARCH_DONE,
        OP_MODIFY_REQUEST => RESPONSE_MODIFY,
        OP_ADD_REQUEST => RESPONSE_ADD,
        OP_DELETE_REQUEST => RESPONSE_DELETE,
        _ => RESPONSE_EXTENDED,
    }
}

/// One decoded BER tag-length-value.
#[cfg(feature = "ldap")]
struct BerElement<'a> {
    tag: u8,
    /// The content octets, already bounds-checked against the input.
    value: &'a [u8],
    /// Tag + length + content, i.e. how far to advance to reach the next element.
    total_len: usize,
}

/// Decode the element at the start of `data`, verifying it fits.
#[cfg(feature = "ldap")]
fn read_ber_element(data: &[u8]) -> Result<BerElement<'_>> {
    if data.is_empty() {
        anyhow::bail!("Truncated BER element: no tag");
    }

    let (content_len, len_bytes) = parse_ber_length(&data[1..])?;
    let header_len = 1 + len_bytes;
    let total_len = header_len
        .checked_add(content_len)
        .context("BER length overflow")?;

    if total_len > data.len() {
        anyhow::bail!(
            "Truncated BER element: claims {} content bytes, {} available",
            content_len,
            data.len().saturating_sub(header_len)
        );
    }

    Ok(BerElement {
        tag: data[0],
        value: &data[header_len..total_len],
        total_len,
    })
}

/// Decode a BER length. Returns (length, bytes consumed by the length field).
#[cfg(feature = "ldap")]
fn parse_ber_length(data: &[u8]) -> Result<(usize, usize)> {
    if data.is_empty() {
        anyhow::bail!("Empty data for BER length");
    }

    let first_byte = data[0];

    if first_byte & 0x80 == 0 {
        // Short form: length is in the first byte
        Ok((first_byte as usize, 1))
    } else {
        // Long form: the low 7 bits say how many bytes encode the length
        let num_len_bytes = (first_byte & 0x7F) as usize;
        if num_len_bytes == 0 || num_len_bytes > 4 {
            anyhow::bail!(
                "Invalid BER length encoding ({} length bytes)",
                num_len_bytes
            );
        }

        if data.len() < 1 + num_len_bytes {
            anyhow::bail!("Insufficient data for BER length");
        }

        let mut length = 0usize;
        for byte in &data[1..1 + num_len_bytes] {
            length = (length << 8) | *byte as usize;
        }

        Ok((length, 1 + num_len_bytes))
    }
}

/// Decode INTEGER/ENUMERATED content octets.
#[cfg(feature = "ldap")]
fn ber_integer(value: &[u8]) -> Result<i64> {
    if value.is_empty() {
        anyhow::bail!("Empty BER INTEGER");
    }
    if value.len() > 8 {
        anyhow::bail!("BER INTEGER too wide ({} bytes)", value.len());
    }

    // Sign-extend from the first byte, as BER integers are two's complement.
    let mut result = if value[0] & 0x80 != 0 { -1i64 } else { 0i64 };
    for byte in value {
        result = (result << 8) | *byte as i64;
    }
    Ok(result)
}

/// Decode OCTET STRING content octets as text.
#[cfg(feature = "ldap")]
fn ber_string(value: &[u8]) -> String {
    String::from_utf8_lossy(value).to_string()
}

/// Length of the complete LDAPMessage at the front of `buffer`, if it has all arrived.
///
/// `Ok(None)` means "need more bytes"; `Err` means the stream is not LDAP and the connection
/// should be dropped.
#[cfg(feature = "ldap")]
fn ldap_message_len(buffer: &[u8]) -> Result<Option<usize>> {
    if buffer.is_empty() {
        return Ok(None);
    }
    if buffer[0] != TAG_SEQUENCE {
        anyhow::bail!(
            "Invalid LDAP message: expected SEQUENCE, got tag 0x{:02x}",
            buffer[0]
        );
    }
    if buffer.len() < 2 {
        return Ok(None);
    }

    let first = buffer[1];
    let (content_len, len_bytes) = if first & 0x80 == 0 {
        (first as usize, 1)
    } else {
        let n = (first & 0x7F) as usize;
        if n == 0 || n > 4 {
            anyhow::bail!("Invalid BER length encoding ({} length bytes)", n);
        }
        if buffer.len() < 2 + n {
            return Ok(None);
        }
        let mut value = 0usize;
        for byte in &buffer[2..2 + n] {
            value = (value << 8) | *byte as usize;
        }
        (value, 1 + n)
    };

    let total = 1 + len_bytes + content_len;
    if total > MAX_LDAP_MESSAGE {
        anyhow::bail!(
            "LDAP message of {} bytes exceeds the {} byte cap",
            total,
            MAX_LDAP_MESSAGE
        );
    }
    if buffer.len() < total {
        return Ok(None);
    }
    Ok(Some(total))
}

/// Decode `AttributeList ::= SEQUENCE OF SEQUENCE { type, vals SET OF value }`.
#[cfg(feature = "ldap")]
fn parse_attribute_list(mut data: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    let mut attributes = serde_json::Map::new();
    while let Ok(attribute) = read_ber_element(data) {
        data = &data[attribute.total_len..];
        let (name, values) = parse_partial_attribute(attribute.value);
        if !name.is_empty() {
            attributes.insert(name, serde_json::Value::Array(values));
        }
    }
    attributes
}

/// Decode the inside of `PartialAttribute ::= SEQUENCE { type, vals SET OF value }`.
#[cfg(feature = "ldap")]
fn parse_partial_attribute(data: &[u8]) -> (String, Vec<serde_json::Value>) {
    let Ok(type_element) = read_ber_element(data) else {
        return (String::new(), Vec::new());
    };
    let name = ber_string(type_element.value);

    let mut values = Vec::new();
    if let Ok(set) = read_ber_element(&data[type_element.total_len..]) {
        let mut cursor = set.value;
        while let Ok(value_element) = read_ber_element(cursor) {
            cursor = &cursor[value_element.total_len..];
            values.push(serde_json::Value::String(ber_string(value_element.value)));
        }
    }

    (name, values)
}

/// Render a search filter back into RFC 4515 text for the event.
///
/// Depth-limited: `depth` counts nesting and the renderer stops at MAX_FILTER_DEPTH rather
/// than recursing as deep as a client asks.
#[cfg(feature = "ldap")]
fn render_filter(element: &BerElement<'_>, depth: usize) -> String {
    if depth >= MAX_FILTER_DEPTH {
        return "(...)".to_string();
    }

    let render_children = |data: &[u8], prefix: &str| -> String {
        let mut out = String::from("(");
        out.push_str(prefix);
        let mut cursor = data;
        while let Ok(child) = read_ber_element(cursor) {
            cursor = &cursor[child.total_len..];
            out.push_str(&render_filter(&child, depth + 1));
        }
        out.push(')');
        out
    };

    let render_comparison = |data: &[u8], operator: &str| -> String {
        let Ok(attribute) = read_ber_element(data) else {
            return "(?)".to_string();
        };
        let value = read_ber_element(&data[attribute.total_len..])
            .map(|v| ber_string(v.value))
            .unwrap_or_default();
        format!("({}{}{})", ber_string(attribute.value), operator, value)
    };

    match element.tag {
        0xA0 => render_children(element.value, "&"),
        0xA1 => render_children(element.value, "|"),
        0xA2 => render_children(element.value, "!"),
        0xA3 => render_comparison(element.value, "="),
        0xA5 => render_comparison(element.value, ">="),
        0xA6 => render_comparison(element.value, "<="),
        0xA8 => render_comparison(element.value, "~="),
        0xA4 => {
            // substrings: SEQUENCE { type, SEQUENCE OF CHOICE { initial, any, final } }
            let Ok(attribute) = read_ber_element(element.value) else {
                return "(?)".to_string();
            };
            let mut pattern = String::new();
            if let Ok(parts) = read_ber_element(&element.value[attribute.total_len..]) {
                let mut cursor = parts.value;
                let mut leading_star = true;
                while let Ok(part) = read_ber_element(cursor) {
                    cursor = &cursor[part.total_len..];
                    match part.tag {
                        0x80 => {
                            pattern.push_str(&ber_string(part.value));
                            leading_star = false;
                        }
                        _ => {
                            if leading_star {
                                pattern.push('*');
                                leading_star = false;
                            }
                            pattern.push('*');
                            pattern.push_str(&ber_string(part.value));
                        }
                    }
                }
            }
            format!("({}={}*)", ber_string(attribute.value), pattern)
        }
        0x87 => format!("({}=*)", ber_string(element.value)),
        other => format!("(filter-0x{:02x}=*)", other),
    }
}

/// True when an encoded BindResponse carries resultCode success.
///
/// Reads the message structurally instead of scanning for a 0x61 byte, which the old
/// implementation did and which any DN or diagnostic message containing that byte could fool.
#[cfg(feature = "ldap")]
fn bind_succeeded(response: &[u8]) -> bool {
    let Ok(envelope) = read_ber_element(response) else {
        return false;
    };
    let Ok(message_id) = read_ber_element(envelope.value) else {
        return false;
    };
    let Ok(op) = read_ber_element(&envelope.value[message_id.total_len..]) else {
        return false;
    };
    if op.tag != RESPONSE_BIND {
        return false;
    }
    read_ber_element(op.value)
        .ok()
        .and_then(|result_code| ber_integer(result_code.value).ok())
        .map(|code| code == RESULT_SUCCESS as i64)
        .unwrap_or(false)
}

#[cfg(feature = "ldap")]
fn encode_ber_length(length: usize) -> Vec<u8> {
    if length < 128 {
        vec![length as u8]
    } else if length < 256 {
        vec![0x81, length as u8]
    } else if length < 65536 {
        vec![0x82, (length >> 8) as u8, length as u8]
    } else {
        vec![
            0x83,
            (length >> 16) as u8,
            (length >> 8) as u8,
            length as u8,
        ]
    }
}

#[cfg(feature = "ldap")]
fn encode_ber_integer(value: i32) -> Vec<u8> {
    let mut result = vec![0x02]; // INTEGER tag

    if value >= 0 && value < 128 {
        result.push(0x01); // length
        result.push(value as u8);
    } else {
        result.push(0x04); // length (4 bytes)
        result.extend_from_slice(&value.to_be_bytes());
    }

    result
}

#[cfg(feature = "ldap")]
fn encode_bind_response(msg_id: i32, result_code: u8, diagnostic_message: &str) -> Vec<u8> {
    // BindResponse ::= [APPLICATION 1] SEQUENCE {
    //     resultCode ENUMERATED,
    //     matchedDN LDAPDN,
    //     diagnosticMessage LDAPString,
    //     ... }

    let mut bind_resp = Vec::new();

    // resultCode (ENUMERATED - same encoding as INTEGER)
    bind_resp.push(0x0A); // ENUMERATED tag
    bind_resp.push(0x01); // length
    bind_resp.push(result_code);

    // matchedDN (OCTET STRING)
    bind_resp.push(0x04); // OCTET STRING tag
    bind_resp.push(0x00); // length (empty)

    // diagnosticMessage (OCTET STRING)
    bind_resp.push(0x04); // OCTET STRING tag
    let diag_bytes = diagnostic_message.as_bytes();
    bind_resp.extend_from_slice(&encode_ber_length(diag_bytes.len()));
    bind_resp.extend_from_slice(diag_bytes);

    // Wrap in BindResponse APPLICATION tag [1]
    let mut bind_msg = vec![0x61]; // APPLICATION 1
    bind_msg.extend_from_slice(&encode_ber_length(bind_resp.len()));
    bind_msg.extend_from_slice(&bind_resp);

    // Create LDAPMessage SEQUENCE
    encode_ldap_message(msg_id, bind_msg)
}

#[cfg(feature = "ldap")]
fn encode_search_done(msg_id: i32, result_code: u8, diagnostic_message: &str) -> Vec<u8> {
    // SearchResultDone ::= [APPLICATION 5] LDAPResult

    let mut result = Vec::new();

    // resultCode (ENUMERATED)
    result.push(0x0A);
    result.push(0x01);
    result.push(result_code);

    // matchedDN (empty)
    result.push(0x04);
    result.push(0x00);

    // diagnosticMessage
    result.push(0x04);
    let diag_bytes = diagnostic_message.as_bytes();
    result.extend_from_slice(&encode_ber_length(diag_bytes.len()));
    result.extend_from_slice(diag_bytes);

    // Wrap in SearchResultDone APPLICATION tag [5]
    let mut search_msg = vec![0x65]; // APPLICATION 5
    search_msg.extend_from_slice(&encode_ber_length(result.len()));
    search_msg.extend_from_slice(&result);

    encode_ldap_message(msg_id, search_msg)
}

/// Encode any LDAPResult-shaped response under the given APPLICATION tag.
///
/// BindResponse [1] = 0x61, SearchResultDone [5] = 0x65, ModifyResponse [7] = 0x67,
/// AddResponse [9] = 0x69, DelResponse [11] = 0x6B, ExtendedResponse [24] = 0x78. All share
/// the LDAPResult prefix (resultCode, matchedDN, diagnosticMessage), which is why one encoder
/// serves them all.
#[cfg(feature = "ldap")]
fn encode_ldap_result(
    msg_id: i32,
    response_tag: u8,
    result_code: u8,
    diagnostic_message: &str,
) -> Vec<u8> {
    let mut result = Vec::new();

    // resultCode (ENUMERATED)
    result.push(0x0A);
    result.push(0x01);
    result.push(result_code);

    // matchedDN (empty OCTET STRING)
    result.push(0x04);
    result.push(0x00);

    // diagnosticMessage (OCTET STRING)
    result.push(0x04);
    let diag_bytes = diagnostic_message.as_bytes();
    result.extend_from_slice(&encode_ber_length(diag_bytes.len()));
    result.extend_from_slice(diag_bytes);

    let mut message = vec![response_tag];
    message.extend_from_slice(&encode_ber_length(result.len()));
    message.extend_from_slice(&result);

    encode_ldap_message(msg_id, message)
}

#[cfg(feature = "ldap")]
fn encode_ldap_message(msg_id: i32, protocol_op: Vec<u8>) -> Vec<u8> {
    // LDAPMessage ::= SEQUENCE {
    //     messageID INTEGER,
    //     protocolOp CHOICE { ... }
    // }

    let mut content = Vec::new();
    content.extend_from_slice(&encode_ber_integer(msg_id));
    content.extend_from_slice(&protocol_op);

    let mut message = vec![0x30]; // SEQUENCE tag
    message.extend_from_slice(&encode_ber_length(content.len()));
    message.extend_from_slice(&content);

    message
}

#[cfg(not(feature = "ldap"))]
impl LdapServer {
    pub async fn spawn_with_llm_actions(
        _listen_addr: SocketAddr,
        _llm_client: crate::llm::ollama_client::OllamaClient,
        _app_state: Arc<crate::state::app_state::AppState>,
        _status_tx: mpsc::UnboundedSender<String>,
        _server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        anyhow::bail!("LDAP feature not enabled")
    }
}
