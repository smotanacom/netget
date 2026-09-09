//! DNS server implementation using hickory-server
pub mod actions;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::DnsProtocol;
use crate::state::app_state::AppState;
use actions::DNS_QUERY_EVENT;
use anyhow::Result;
use hickory_proto::op::Message as DnsMessage;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

/// DNS server that integrates with LLM for query handling
pub struct DnsServer;

impl DnsServer {
    /// Spawn DNS server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        let socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("DNS server listening on {}", local_addr));

        let protocol = Arc::new(DnsProtocol::new());

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            // RFC 1035 caps plain UDP DNS at 512 bytes, but EDNS0 (RFC 6891)
            // clients routinely advertise 1232-4096 byte buffers. A short
            // buffer would silently truncate those datagrams, and the truncated
            // bytes would then fail to parse as DNS.
            let mut buffer = vec![0u8; 4096];

            loop {
                match socket.recv_from(&mut buffer).await {
                    Ok((n, peer_addr)) => {
                        let data = buffer[..n].to_vec();
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);

                        // Add connection to ServerInstance (DNS "connection" = recent query)
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr: peer_addr,
                            local_addr,
                            bytes_sent: 0,
                            bytes_received: n as u64,
                            packets_sent: 0,
                            packets_received: 1,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        // Datagram summary + full payload are FileOnly: the dns_query
                        // event template surfaces the query to the TUI, so streaming the
                        // raw datagram too would duplicate it on the unbounded channel.
                        {
                            let log = Log::new(Some(&status_tx));
                            log.debug(format!("DNS received {} bytes from {}", n, peer_addr));
                            log.trace(format!("DNS data (hex): {}", hex::encode(&data)));
                        }

                        let llm_clone = llm_client.clone();
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let socket_clone = socket.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let log = Log::new(Some(&status_clone));
                            // Parse DNS query using hickory-proto
                            match DnsMessage::from_vec(&data) {
                                Ok(query) => {
                                    // Extract query information
                                    let query_id = query.id();
                                    let queries = query.queries();

                                    let mut query_descriptions = Vec::new();
                                    for q in queries {
                                        let qname = q.name().to_string();
                                        let qtype = q.query_type();
                                        let qclass = q.query_class();
                                        query_descriptions.push(format!(
                                            "{} {} {} (ID: {})",
                                            qname, qtype, qclass, query_id
                                        ));

                                        // Parsed-query summary (FileOnly; the dns_query
                                        // event template covers the TUI).
                                        log.debug(format!(
                                            "DNS query: {} {} {}",
                                            qname, qtype, qclass
                                        ));
                                    }

                                    // Create DNS query event
                                    let first_query = queries.first();
                                    let domain = first_query
                                        .map(|q| q.name().to_string())
                                        .unwrap_or_default();
                                    let query_type = first_query
                                        .map(|q| q.query_type().to_string())
                                        .unwrap_or_default();

                                    let event = Event::new(
                                        &DNS_QUERY_EVENT,
                                        serde_json::json!({
                                            "query_id": query_id,
                                            "domain": domain,
                                            "query_type": query_type
                                        }),
                                    );

                                    log.debug(format!(
                                        "DNS calling LLM for query from {}",
                                        peer_addr
                                    ));

                                    // `Some(connection_id)`, not `None`. The connection was
                                    // registered above, and this argument is what
                                    // `call_llm_inner` uses to look up the peer address for
                                    // `EventLogContext` — so with `None` the `dns_query`
                                    // event's own INFO template, "DNS {query_type} {domain}
                                    // from {client_ip}", rendered every access-log line as
                                    // "... from " with nothing after it. It is also what
                                    // scopes connection-scoped handlers and tasks.
                                    match call_llm(
                                        &llm_clone,
                                        &state_clone,
                                        server_id,
                                        Some(connection_id),
                                        &event,
                                        protocol_clone.as_ref(),
                                    )
                                    .await
                                    {
                                        Ok(execution_result) => {
                                            // Display messages from LLM
                                            for message in &execution_result.messages {
                                                log.info(message);
                                            }

                                            log.debug(format!(
                                                "DNS got {} protocol results",
                                                execution_result.protocol_results.len()
                                            ));

                                            for protocol_result in execution_result.protocol_results
                                            {
                                                if let Some(output_data) =
                                                    protocol_result.get_all_output().first()
                                                {
                                                    let _ = socket_clone
                                                        .send_to(output_data, peer_addr)
                                                        .await;

                                                    // Keep the connection counters shown in the
                                                    // TUI in step with what was actually sent.
                                                    state_clone
                                                        .update_connection_stats(
                                                            server_id,
                                                            connection_id,
                                                            None,
                                                            Some(output_data.len() as u64),
                                                            None,
                                                            Some(1),
                                                        )
                                                        .await;

                                                    // Sent summary + payload are FileOnly;
                                                    // the access line below carries the TUI.
                                                    log.debug(format!(
                                                        "DNS sent {} bytes to {}",
                                                        output_data.len(),
                                                        peer_addr
                                                    ));
                                                    log.trace(format!(
                                                        "DNS sent (hex): {}",
                                                        hex::encode(output_data)
                                                    ));

                                                    log.info(format!(
                                                        "DNS response to {} ({} bytes)",
                                                        peer_addr,
                                                        output_data.len()
                                                    ));
                                                } else {
                                                    log.debug(
                                                        "DNS protocol result has no output data",
                                                    );
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            // Answer SERVFAIL rather than dropping the query.
                                            //
                                            // Silence here costs the client its full per-server
                                            // timeout (5s in glibc) before it tries anywhere
                                            // else; SERVFAIL makes it move on at once. The
                                            // query ID and question section are echoed, without
                                            // which a stub resolver discards the packet and we
                                            // are back to silence.
                                            // `decision=` tag, as `src/server/radius/`
                                            // does it: SERVFAIL is the same bytes whatever
                                            // went wrong, so the log is the only place the
                                            // distinction can survive.
                                            let decision = if crate::llm::is_overload_error(&e) {
                                                "fail_closed_llm_overload"
                                            } else {
                                                "fail_closed_llm_error"
                                            };
                                            log.warn(format!(
                                                "DNS LLM call failed for query from {} ({}) \
                                                 decision={}: {}",
                                                peer_addr, connection_id, decision, e
                                            ));

                                            match actions::build_servfail(&query) {
                                                Ok(packet) => {
                                                    let _ = socket_clone
                                                        .send_to(&packet, peer_addr)
                                                        .await;
                                                    state_clone
                                                        .update_connection_stats(
                                                            server_id,
                                                            connection_id,
                                                            None,
                                                            Some(packet.len() as u64),
                                                            None,
                                                            Some(1),
                                                        )
                                                        .await;
                                                    log.info(format!(
                                                        "DNS SERVFAIL to {} ({} bytes)",
                                                        peer_addr,
                                                        packet.len()
                                                    ));
                                                }
                                                Err(build_err) => {
                                                    log.error(format!(
                                                        "DNS failed to build SERVFAIL for {}: {}",
                                                        peer_addr, build_err
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    log.warn(format!("Failed to parse DNS query: {e}"));

                                    // Fall back to hex representation for malformed queries
                                    let hex_str = hex::encode(&data);
                                    log.debug(format!(
                                        "DNS malformed query from {} ({} bytes, hex: {})",
                                        peer_addr,
                                        data.len(),
                                        hex_str
                                    ));
                                }
                            }
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("DNS receive error: {}", e));
                        break;
                    }
                }
            }
        });

        // Register the recv loop so stop_server can abort it and release the port.
        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}
