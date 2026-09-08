//! TFTP server implementation
pub mod actions;

use crate::server::connection::ConnectionId;
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};
use tracing::error;

use crate::llm::action_helper::call_llm;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::TftpProtocol;
use crate::state::app_state::AppState;
use actions::{
    TFTP_ACK_RECEIVED_EVENT, TFTP_DATA_BLOCK_EVENT, TFTP_READ_REQUEST_EVENT,
    TFTP_WRITE_REQUEST_EVENT,
};

/// RFC 1350 opcodes. Named rather than spelled as literals at each comparison, because the
/// read continuation used to test a packet's *length* where it meant to test its opcode.
const OP_DATA: u16 = 3;
const OP_ERROR: u16 = 5;

/// How many consecutive `recv_from` failures on the main socket are tolerated before the
/// accept loop gives up.
///
/// One error is routine on UDP: an ICMP port-unreachable provoked by a datagram we sent to a
/// client that has gone away is delivered as an error on the *next* receive. A permanent one
/// is not, and `continue`ing on it turns this loop into a busy-wait that also floods the
/// unbounded status channel.
const MAX_CONSECUTIVE_SOCKET_ERRORS: u32 = 64;

/// Transfer ID (client address + transaction ID port)
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct TransferId {
    client_addr: SocketAddr,
}

impl TransferId {
    fn new(client_addr: SocketAddr) -> Self {
        Self { client_addr }
    }
}

/// TFTP transfer operation type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TftpOperation {
    Read,  // Server sends DATA, receives ACK
    Write, // Server receives DATA, sends ACK
}

/// Transfer state
#[allow(dead_code)]
struct TftpTransfer {
    transfer_id: TransferId,
    operation: TftpOperation,
    filename: String,
    mode: String,
    current_block: u16,
    connection_id: ConnectionId,
    server_socket: Arc<UdpSocket>, // Unique socket for this transfer (TID)
}

/// TFTP server that forwards requests to LLM
pub struct TftpServer;

impl TftpServer {
    /// Spawn TFTP server with integrated LLM actions
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> Result<SocketAddr> {
        // Main listening socket (port 69)
        let main_socket = Arc::new(UdpSocket::bind(listen_addr).await?);
        let local_addr = main_socket.local_addr()?;
        Log::new(Some(&status_tx)).info(format!("TFTP server listening on {}", local_addr));

        let protocol = Arc::new(TftpProtocol::new());

        // Active transfers (keyed by TransferId)
        let transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Spawn main request listener (handles RRQ and WRQ)
        let main_socket_clone = main_socket.clone();
        let llm_clone = llm_client.clone();
        let state_clone = app_state.clone();
        let status_clone = status_tx.clone();
        let protocol_clone = protocol.clone();
        let transfers_clone = transfers.clone();

        let accept_handle = tokio::spawn(async move {
            Self::handle_main_requests(
                main_socket_clone,
                llm_clone,
                state_clone,
                status_clone,
                server_id,
                protocol_clone,
                transfers_clone,
            )
            .await;
        });

        app_state
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }

    /// Handle incoming requests on main socket (RRQ and WRQ)
    async fn handle_main_requests(
        main_socket: Arc<UdpSocket>,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: Arc<TftpProtocol>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) {
        let mut buffer = vec![0u8; 516]; // Max TFTP packet (4 bytes header + 512 data)
        let log = Log::new(Some(&status_tx));
        let mut consecutive_errors: u32 = 0;

        loop {
            match main_socket.recv_from(&mut buffer).await {
                Ok((n, peer_addr)) => {
                    consecutive_errors = 0;
                    let data = buffer[..n].to_vec();

                    // Summary + full payload FileOnly: the tftp_*_request event
                    // templates render the equivalent lines to the TUI.
                    log.debug(format!("TFTP received {} bytes from {}", n, peer_addr));
                    log.trace(format!("TFTP packet (hex): {}", hex::encode(&data)));

                    // Parse TFTP opcode
                    if data.len() < 2 {
                        log.debug("TFTP packet too short, ignoring");
                        continue;
                    }

                    let opcode = u16::from_be_bytes([data[0], data[1]]);

                    match opcode {
                        1 => {
                            // RRQ (Read Request)
                            let llm_clone = llm_client.clone();
                            let state_clone = app_state.clone();
                            let status_clone = status_tx.clone();
                            let protocol_clone = protocol.clone();
                            let transfers_clone = transfers.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_read_request(
                                    data,
                                    peer_addr,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    server_id,
                                    protocol_clone,
                                    transfers_clone,
                                )
                                .await
                                {
                                    error!("TFTP RRQ handling error: {}", e);
                                }
                            });
                        }
                        2 => {
                            // WRQ (Write Request)
                            let llm_clone = llm_client.clone();
                            let state_clone = app_state.clone();
                            let status_clone = status_tx.clone();
                            let protocol_clone = protocol.clone();
                            let transfers_clone = transfers.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_write_request(
                                    data,
                                    peer_addr,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    server_id,
                                    protocol_clone,
                                    transfers_clone,
                                )
                                .await
                                {
                                    error!("TFTP WRQ handling error: {}", e);
                                }
                            });
                        }
                        _ => {
                            log.debug(format!(
                                "TFTP unexpected opcode {} on main socket, ignoring",
                                opcode
                            ));
                        }
                    }
                }
                Err(e) => {
                    // A UDP socket reports transient per-datagram errors here — on Linux an
                    // ICMP port-unreachable from a client that has gone away surfaces as
                    // ECONNREFUSED on the *next* recv — so a single error must not stop the
                    // server. But `continue`ing on a permanent one (the socket closed under
                    // us) spins this loop at 100% CPU and floods the unbounded status
                    // channel, which is what it did. Tolerate a burst, then give up.
                    consecutive_errors += 1;
                    log.error(format!(
                        "TFTP main socket error ({consecutive_errors} in a row): {e}"
                    ));
                    if consecutive_errors >= MAX_CONSECUTIVE_SOCKET_ERRORS {
                        log.error(format!(
                            "TFTP main socket failed {MAX_CONSECUTIVE_SOCKET_ERRORS} times in a \
                             row; stopping the accept loop rather than spinning on it"
                        ));
                        return;
                    }
                }
            }
        }
    }

    /// Handle RRQ (Read Request)
    async fn handle_read_request(
        data: Vec<u8>,
        peer_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: Arc<TftpProtocol>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) -> Result<()> {
        // Parse RRQ: opcode(2) filename(string) 0 mode(string) 0
        let (filename, mode) = Self::parse_request(&data[2..])?;
        let log = Log::new(Some(&status_tx));

        log.debug(format!(
            "TFTP RRQ: filename='{}' mode='{}' from {}",
            filename, mode, peer_addr
        ));

        // Create unique socket for this transfer (transaction ID)
        let transfer_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        let tid_addr = transfer_socket.local_addr()?;

        log.debug(format!("TFTP RRQ assigned TID {}", tid_addr.port()));

        // Create connection entry
        let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr: tid_addr,
            bytes_sent: 0,
            bytes_received: data.len() as u64,
            packets_sent: 0,
            packets_received: 1,
            last_activity: now,
            status: ConnectionStatus::Active,
            status_changed_at: now,
            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                "operation": "read",
                "filename": filename.clone(),
                "current_block": 0,
                "total_bytes": 0,
            })),
        };
        app_state
            .add_connection_to_server(server_id, conn_state)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Create transfer state
        let transfer_id = TransferId::new(peer_addr);
        let transfer = TftpTransfer {
            transfer_id,
            operation: TftpOperation::Read,
            filename: filename.clone(),
            mode: mode.clone(),
            current_block: 0,
            connection_id,
            server_socket: transfer_socket.clone(),
        };

        transfers.lock().await.insert(transfer_id, transfer);

        // Call LLM with read request event
        let event = Event::new(
            &TFTP_READ_REQUEST_EVENT,
            serde_json::json!({
                "filename": filename,
                "mode": mode,
                "client_addr": peer_addr.to_string(),
            }),
        );

        log.debug(format!("TFTP calling LLM for RRQ: {}", filename));

        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                // Display messages
                for message in &execution_result.messages {
                    log.info(message);
                }

                log.debug(format!(
                    "TFTP parsed {} actions for RRQ",
                    execution_result.raw_actions.len()
                ));

                // Process protocol results (DATA or ERROR packets)
                for protocol_result in execution_result.protocol_results {
                    match protocol_result {
                        crate::llm::actions::protocol_trait::ActionResult::Output(packet) => {
                            // Send packet from transfer socket to client
                            if let Err(e) = transfer_socket.send_to(&packet, peer_addr).await {
                                log.error(format!("TFTP failed to send DATA: {}", e));
                                continue;
                            }

                            // Update bytes sent
                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    None,
                                    Some(packet.len() as u64),
                                    None,
                                    Some(1),
                                )
                                .await;

                            log.trace(format!(
                                "TFTP sent DATA packet (hex): {}",
                                hex::encode(&packet)
                            ));

                            // Check if this is DATA packet
                            if packet.len() >= 4 {
                                let opcode = u16::from_be_bytes([packet[0], packet[1]]);
                                if opcode == 3 {
                                    // DATA
                                    let block_num = u16::from_be_bytes([packet[2], packet[3]]);
                                    let data_len = packet.len() - 4;

                                    // Update transfer state
                                    if let Some(transfer) =
                                        transfers.lock().await.get_mut(&transfer_id)
                                    {
                                        transfer.current_block = block_num;
                                    }

                                    // Update connection info
                                    app_state
                                        .update_tftp_connection_block(
                                            server_id,
                                            connection_id,
                                            block_num,
                                            data_len,
                                        )
                                        .await;

                                    log.debug(format!(
                                        "TFTP sent DATA block {} ({} bytes)",
                                        block_num, data_len
                                    ));

                                    // If final block (< 512 bytes data), spawn ACK listener
                                    if data_len < 512 {
                                        log.debug("TFTP final block sent, waiting for final ACK");

                                        // Spawn listener for final ACK
                                        let socket_clone = transfer_socket.clone();
                                        let status_clone = status_tx.clone();
                                        let state_clone = app_state.clone();
                                        let transfers_clone = transfers.clone();

                                        tokio::spawn(async move {
                                            Self::wait_for_final_ack(
                                                socket_clone,
                                                peer_addr,
                                                transfer_id,
                                                block_num,
                                                server_id,
                                                connection_id,
                                                state_clone,
                                                status_clone,
                                                transfers_clone,
                                            )
                                            .await;
                                        });
                                    } else {
                                        // Spawn listener for ACK and continue transfer
                                        let socket_clone = transfer_socket.clone();
                                        let llm_clone = llm_client.clone();
                                        let state_clone = app_state.clone();
                                        let status_clone = status_tx.clone();
                                        let protocol_clone = protocol.clone();
                                        let transfers_clone = transfers.clone();

                                        tokio::spawn(async move {
                                            Self::continue_read_transfer(
                                                socket_clone,
                                                peer_addr,
                                                transfer_id,
                                                block_num,
                                                llm_clone,
                                                state_clone,
                                                status_clone,
                                                server_id,
                                                connection_id,
                                                protocol_clone,
                                                transfers_clone,
                                            )
                                            .await;
                                        });
                                    }
                                } else if opcode == 5 {
                                    // ERROR - transfer terminated
                                    log.debug("TFTP sent ERROR, transfer terminated");
                                    transfers.lock().await.remove(&transfer_id);
                                    app_state
                                        .close_connection_on_server(server_id, connection_id)
                                        .await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                Self::fail_transfer_on_llm_error(
                    &transfer_socket,
                    peer_addr,
                    transfer_id,
                    "read request",
                    &e,
                    server_id,
                    connection_id,
                    &app_state,
                    &status_tx,
                    &transfers,
                )
                .await;
            }
        }

        Ok(())
    }

    /// Continue read transfer by waiting for ACK and calling LLM for next block
    fn continue_read_transfer(
        socket: Arc<UdpSocket>,
        peer_addr: SocketAddr,
        transfer_id: TransferId,
        expected_block: u16,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        protocol: Arc<TftpProtocol>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(async move {
            let mut buffer = vec![0u8; 516];
            let log = Log::new(Some(&status_tx));

            // Wait for ACK with timeout
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                socket.recv_from(&mut buffer),
            )
            .await
            {
                Ok(Ok((n, _))) => {
                    let data = &buffer[..n];

                    if data.len() >= 4 {
                        let opcode = u16::from_be_bytes([data[0], data[1]]);
                        if opcode == 4 {
                            // ACK
                            let ack_block = u16::from_be_bytes([data[2], data[3]]);

                            log.debug(format!("TFTP received ACK for block {}", ack_block));

                            if ack_block == expected_block {
                                // Update connection
                                app_state
                                    .update_connection_stats(
                                        server_id,
                                        connection_id,
                                        Some(n as u64),
                                        None,
                                        Some(1),
                                        None,
                                    )
                                    .await;

                                // Call LLM for next block
                                let event = Event::new(
                                    &TFTP_ACK_RECEIVED_EVENT,
                                    serde_json::json!({
                                        "block_number": ack_block,
                                    }),
                                );

                                match call_llm(
                                    &llm_client,
                                    &app_state,
                                    server_id,
                                    Some(connection_id),
                                    &event,
                                    protocol.as_ref(),
                                )
                                .await
                                {
                                    Ok(execution_result) => {
                                        // Exactly one continuation may be spawned per answer.
                                        // The model can legitimately return several actions,
                                        // and spawning a reader per packet would put two or
                                        // more tasks on the same socket racing for the next
                                        // ACK.
                                        let mut continued = false;
                                        for protocol_result in execution_result.protocol_results {
                                            if let crate::llm::actions::protocol_trait::ActionResult::Output(packet) = protocol_result {
                                            if let Err(e) = socket.send_to(&packet, peer_addr).await {
                                                log.error(format!(
                                                    "TFTP failed to send packet to {peer_addr}: {e}"
                                                ));
                                                continue;
                                            }

                                            app_state.update_connection_stats(server_id, connection_id, None, Some(packet.len() as u64), None, Some(1)).await;

                                            if continued {
                                                continue;
                                            }

                                            // Dispatch on the opcode, as the RRQ path does.
                                            // This branch used to test only the length, so an
                                            // ERROR packet — which is short — was taken for a
                                            // final DATA block: the server waited five seconds
                                            // for an ACK the client had no reason to send, and
                                            // read the packet's *error code* as a block number.
                                            // Aborting a transfer mid-flight therefore looked
                                            // to the log like a completed one.
                                            match Self::packet_opcode(&packet) {
                                                Some(OP_DATA) => {
                                                    let block_num = u16::from_be_bytes([packet[2], packet[3]]);
                                                    let data_len = packet.len() - 4;
                                                    continued = true;

                                                    let socket_clone = socket.clone();
                                                    let state_clone = app_state.clone();
                                                    let status_clone = status_tx.clone();
                                                    let transfers_clone = transfers.clone();

                                                    if data_len < 512 {
                                                        log.debug("TFTP final block sent");
                                                        tokio::spawn(async move {
                                                            Self::wait_for_final_ack(
                                                                socket_clone,
                                                                peer_addr,
                                                                transfer_id,
                                                                block_num,
                                                                server_id,
                                                                connection_id,
                                                                state_clone,
                                                                status_clone,
                                                                transfers_clone,
                                                            )
                                                            .await;
                                                        });
                                                    } else {
                                                        let llm_clone = llm_client.clone();
                                                        let protocol_clone = protocol.clone();
                                                        tokio::spawn(async move {
                                                            Self::continue_read_transfer(
                                                                socket_clone,
                                                                peer_addr,
                                                                transfer_id,
                                                                block_num,
                                                                llm_clone,
                                                                state_clone,
                                                                status_clone,
                                                                server_id,
                                                                connection_id,
                                                                protocol_clone,
                                                                transfers_clone,
                                                            )
                                                            .await;
                                                        });
                                                    }
                                                }
                                                Some(OP_ERROR) => {
                                                    log.debug(
                                                        "TFTP sent ERROR mid-transfer, transfer terminated",
                                                    );
                                                    continued = true;
                                                    transfers.lock().await.remove(&transfer_id);
                                                    app_state
                                                        .close_connection_on_server(server_id, connection_id)
                                                        .await;
                                                }
                                                other => {
                                                    log.warn(format!(
                                                        "TFTP handler answered a read transfer with opcode \
                                                         {other:?}, which is neither DATA nor ERROR; the \
                                                         transfer cannot continue"
                                                    ));
                                                }
                                            }
                                        }
                                        }
                                    }
                                    Err(e) => {
                                        // Mid-transfer failure: the client is waiting for
                                        // the next DATA block, so it gets an ERROR packet
                                        // instead of nothing.
                                        Self::fail_transfer_on_llm_error(
                                            &socket,
                                            peer_addr,
                                            transfer_id,
                                            "read transfer",
                                            &e,
                                            server_id,
                                            connection_id,
                                            &app_state,
                                            &status_tx,
                                            &transfers,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    log.error(format!("TFTP socket error waiting for ACK: {}", e));
                    transfers.lock().await.remove(&transfer_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                }
                Err(_) => {
                    log.debug(format!(
                        "TFTP timeout waiting for ACK block {}",
                        expected_block
                    ));
                    transfers.lock().await.remove(&transfer_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                }
            }
        })
    }

    /// Wait for final ACK and close transfer
    async fn wait_for_final_ack(
        socket: Arc<UdpSocket>,
        _peer_addr: SocketAddr,
        transfer_id: TransferId,
        expected_block: u16,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) {
        let mut buffer = vec![0u8; 516];
        let log = Log::new(Some(&status_tx));

        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.recv_from(&mut buffer),
        )
        .await
        {
            Ok(Ok((n, _))) => {
                let data = &buffer[..n];

                if data.len() >= 4 {
                    let opcode = u16::from_be_bytes([data[0], data[1]]);
                    if opcode == 4 {
                        let ack_block = u16::from_be_bytes([data[2], data[3]]);

                        if ack_block == expected_block {
                            log.debug(format!(
                                "TFTP transfer complete (final ACK received for block {})",
                                ack_block
                            ));

                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    Some(n as u64),
                                    None,
                                    Some(1),
                                    None,
                                )
                                .await;

                            transfers.lock().await.remove(&transfer_id);
                            app_state
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                        }
                    }
                }
            }
            _ => {
                log.debug("TFTP timeout waiting for final ACK (transfer may have completed)");
                transfers.lock().await.remove(&transfer_id);
                app_state
                    .close_connection_on_server(server_id, connection_id)
                    .await;
            }
        }
    }

    /// Handle WRQ (Write Request)
    async fn handle_write_request(
        data: Vec<u8>,
        peer_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        protocol: Arc<TftpProtocol>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) -> Result<()> {
        // Parse WRQ
        let (filename, mode) = Self::parse_request(&data[2..])?;
        let log = Log::new(Some(&status_tx));

        log.debug(format!(
            "TFTP WRQ: filename='{}' mode='{}' from {}",
            filename, mode, peer_addr
        ));

        // Create unique socket for this transfer
        let transfer_socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        let tid_addr = transfer_socket.local_addr()?;

        log.debug(format!("TFTP WRQ assigned TID {}", tid_addr.port()));

        // Create connection entry
        let connection_id = ConnectionId::new(app_state.get_next_unified_id().await);
        use crate::state::server::{
            ConnectionState as ServerConnectionState, ConnectionStatus, ProtocolConnectionInfo,
        };
        let now = std::time::Instant::now();
        let conn_state = ServerConnectionState {
            id: connection_id,
            remote_addr: peer_addr,
            local_addr: tid_addr,
            bytes_sent: 0,
            bytes_received: data.len() as u64,
            packets_sent: 0,
            packets_received: 1,
            last_activity: now,
            status: ConnectionStatus::Active,
            status_changed_at: now,
            protocol_info: ProtocolConnectionInfo::new(serde_json::json!({
                "operation": "write",
                "filename": filename.clone(),
                "current_block": 0,
                "total_bytes": 0,
            })),
        };
        app_state
            .add_connection_to_server(server_id, conn_state)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Create transfer state
        let transfer_id = TransferId::new(peer_addr);
        let transfer = TftpTransfer {
            transfer_id,
            operation: TftpOperation::Write,
            filename: filename.clone(),
            mode: mode.clone(),
            current_block: 0,
            connection_id,
            server_socket: transfer_socket.clone(),
        };

        transfers.lock().await.insert(transfer_id, transfer);

        // Call LLM with write request event
        let event = Event::new(
            &TFTP_WRITE_REQUEST_EVENT,
            serde_json::json!({
                "filename": filename,
                "mode": mode,
                "client_addr": peer_addr.to_string(),
            }),
        );

        log.debug(format!("TFTP calling LLM for WRQ: {}", filename));

        match call_llm(
            &llm_client,
            &app_state,
            server_id,
            Some(connection_id),
            &event,
            protocol.as_ref(),
        )
        .await
        {
            Ok(execution_result) => {
                // Display messages
                for message in &execution_result.messages {
                    log.info(message);
                }

                // Process protocol results (should be ACK block 0 or ERROR)
                for protocol_result in execution_result.protocol_results {
                    match protocol_result {
                        crate::llm::actions::protocol_trait::ActionResult::Output(packet) => {
                            // Send ACK block 0 or ERROR
                            if let Err(e) = transfer_socket.send_to(&packet, peer_addr).await {
                                log.error(format!("TFTP failed to send ACK: {}", e));
                                continue;
                            }

                            app_state
                                .update_connection_stats(
                                    server_id,
                                    connection_id,
                                    None,
                                    Some(packet.len() as u64),
                                    None,
                                    Some(1),
                                )
                                .await;

                            log.trace(format!("TFTP sent packet (hex): {}", hex::encode(&packet)));

                            // Check if ACK (opcode 4)
                            if packet.len() >= 4 {
                                let opcode = u16::from_be_bytes([packet[0], packet[1]]);
                                if opcode == 4 {
                                    log.debug("TFTP sent ACK block 0, ready to receive");

                                    // Spawn listener for incoming DATA blocks
                                    let socket_clone = transfer_socket.clone();
                                    let llm_clone = llm_client.clone();
                                    let state_clone = app_state.clone();
                                    let status_clone = status_tx.clone();
                                    let protocol_clone = protocol.clone();
                                    let transfers_clone = transfers.clone();

                                    tokio::spawn(async move {
                                        Self::receive_write_data(
                                            socket_clone,
                                            peer_addr,
                                            transfer_id,
                                            llm_clone,
                                            state_clone,
                                            status_clone,
                                            server_id,
                                            connection_id,
                                            protocol_clone,
                                            transfers_clone,
                                        )
                                        .await;
                                    });
                                } else if opcode == 5 {
                                    log.debug("TFTP sent ERROR, transfer denied");
                                    transfers.lock().await.remove(&transfer_id);
                                    app_state
                                        .close_connection_on_server(server_id, connection_id)
                                        .await;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                Self::fail_transfer_on_llm_error(
                    &transfer_socket,
                    peer_addr,
                    transfer_id,
                    "write request",
                    &e,
                    server_id,
                    connection_id,
                    &app_state,
                    &status_tx,
                    &transfers,
                )
                .await;
            }
        }

        Ok(())
    }

    /// Receive DATA blocks for write transfer
    async fn receive_write_data(
        socket: Arc<UdpSocket>,
        peer_addr: SocketAddr,
        transfer_id: TransferId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        protocol: Arc<TftpProtocol>,
        transfers: Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) {
        let mut buffer = vec![0u8; 516];
        let log = Log::new(Some(&status_tx));

        loop {
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                socket.recv_from(&mut buffer),
            )
            .await
            {
                Ok(Ok((n, _))) => {
                    let data = buffer[..n].to_vec();

                    if data.len() < 4 {
                        log.debug("TFTP received short packet, ignoring");
                        continue;
                    }

                    let opcode = u16::from_be_bytes([data[0], data[1]]);
                    if opcode != 3 {
                        log.debug(format!(
                            "TFTP expected DATA (opcode 3), got opcode {}",
                            opcode
                        ));
                        continue;
                    }

                    let block_num = u16::from_be_bytes([data[2], data[3]]);
                    let block_data = &data[4..];

                    // Summary FileOnly: the tftp_data_block event template renders
                    // the equivalent line to the TUI.
                    log.debug(format!(
                        "TFTP received DATA block {} ({} bytes)",
                        block_num,
                        block_data.len()
                    ));

                    app_state
                        .update_connection_stats(
                            server_id,
                            connection_id,
                            Some(n as u64),
                            None,
                            Some(1),
                            None,
                        )
                        .await;

                    // Update transfer state
                    if let Some(transfer) = transfers.lock().await.get_mut(&transfer_id) {
                        transfer.current_block = block_num;
                    }

                    app_state
                        .update_tftp_connection_block(
                            server_id,
                            connection_id,
                            block_num,
                            block_data.len(),
                        )
                        .await;

                    // Call LLM with data block event
                    let is_final = block_data.len() < 512;
                    let event = Event::new(
                        &TFTP_DATA_BLOCK_EVENT,
                        serde_json::json!({
                            "block_number": block_num,
                            "data_hex": hex::encode(block_data),
                            "data_length": block_data.len(),
                            "is_final": is_final,
                        }),
                    );

                    match call_llm(
                        &llm_client,
                        &app_state,
                        server_id,
                        Some(connection_id),
                        &event,
                        protocol.as_ref(),
                    )
                    .await
                    {
                        Ok(execution_result) => {
                            // Process ACK packets
                            let mut aborted = false;
                            for protocol_result in execution_result.protocol_results {
                                if let crate::llm::actions::protocol_trait::ActionResult::Output(
                                    packet,
                                ) = protocol_result
                                {
                                    if let Err(e) = socket.send_to(&packet, peer_addr).await {
                                        log.error(format!(
                                            "TFTP failed to send packet to {peer_addr}: {e}"
                                        ));
                                        continue;
                                    }

                                    app_state
                                        .update_connection_stats(
                                            server_id,
                                            connection_id,
                                            None,
                                            Some(packet.len() as u64),
                                            None,
                                            Some(1),
                                        )
                                        .await;

                                    // The handler is allowed to abort an upload with
                                    // send_tftp_error. That used to be written to the wire and
                                    // then ignored: the loop went straight back to waiting for
                                    // the next DATA block, so a refused upload kept its socket
                                    // and its connection entry until the ten-second timeout.
                                    if Self::packet_opcode(&packet) == Some(OP_ERROR) {
                                        log.debug(
                                            "TFTP sent ERROR during upload, transfer terminated",
                                        );
                                        aborted = true;
                                    } else {
                                        log.debug(format!("TFTP sent ACK for block {}", block_num));
                                    }
                                }
                            }

                            if aborted {
                                transfers.lock().await.remove(&transfer_id);
                                app_state
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                                break;
                            }

                            // If final block, close transfer
                            if is_final {
                                log.debug(format!(
                                    "TFTP write transfer complete (final block {} received)",
                                    block_num
                                ));
                                transfers.lock().await.remove(&transfer_id);
                                app_state
                                    .close_connection_on_server(server_id, connection_id)
                                    .await;
                                break;
                            }
                        }
                        Err(e) => {
                            // The client is waiting for an ACK for the block it just
                            // sent; answer with ERROR rather than leaving it to retransmit
                            // into a socket nobody is reading any more.
                            Self::fail_transfer_on_llm_error(
                                &socket,
                                peer_addr,
                                transfer_id,
                                "write data block",
                                &e,
                                server_id,
                                connection_id,
                                &app_state,
                                &status_tx,
                                &transfers,
                            )
                            .await;
                            break;
                        }
                    }
                }
                Ok(Err(e)) => {
                    log.error(format!("TFTP socket error: {}", e));
                    transfers.lock().await.remove(&transfer_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    break;
                }
                Err(_) => {
                    log.debug("TFTP timeout waiting for DATA");
                    transfers.lock().await.remove(&transfer_id);
                    app_state
                        .close_connection_on_server(server_id, connection_id)
                        .await;
                    break;
                }
            }
        }
    }

    /// The opcode of a packet the handler produced, or `None` if it is too short to have one.
    ///
    /// Every action this protocol defines yields at least four bytes, so `None` means an
    /// action outside the declared set produced it — worth logging rather than indexing into.
    fn packet_opcode(packet: &[u8]) -> Option<u16> {
        if packet.len() < 4 {
            return None;
        }
        Some(u16::from_be_bytes([packet[0], packet[1]]))
    }

    /// Parse RRQ/WRQ request packet
    fn parse_request(data: &[u8]) -> Result<(String, String)> {
        // Format: filename\0mode\0
        let null_positions: Vec<usize> = data
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| if b == 0 { Some(i) } else { None })
            .collect();

        if null_positions.len() < 2 {
            return Err(anyhow::anyhow!("Invalid TFTP request format"));
        }

        let filename = String::from_utf8_lossy(&data[..null_positions[0]]).to_string();
        let mode =
            String::from_utf8_lossy(&data[null_positions[0] + 1..null_positions[1]]).to_string();

        Ok((filename, mode))
    }

    /// Build the ERROR packet a client gets when the LLM call behind a transfer fails.
    ///
    /// RFC 1350 defines error codes 0-7 and none of them means "try again", so both cases
    /// use code 0 ("Not defined, see error message") and the message says which. The
    /// packet is never a plausible-looking DATA or ACK: an LLM outage must not be
    /// indistinguishable from a successful transfer.
    fn llm_failure_error_packet(err: &anyhow::Error) -> Vec<u8> {
        if crate::llm::is_overload_error(err) {
            Self::build_error_packet(0, "Server overloaded, retry later")
        } else {
            Self::build_error_packet(0, "Internal error: LLM backend failure")
        }
    }

    /// Answer an LLM failure with an ERROR packet and tear the transfer down.
    ///
    /// Every LLM call in a TFTP transfer routes its error branch through here. Two of the
    /// four used to write nothing at all, so the client sat in `recvfrom` until its own
    /// retransmit/timeout logic gave up with no indication of what went wrong - a partial
    /// transfer that simply stopped, which for a PXE boot looks like a corrupt image.
    #[allow(clippy::too_many_arguments)]
    async fn fail_transfer_on_llm_error(
        socket: &UdpSocket,
        peer_addr: SocketAddr,
        transfer_id: TransferId,
        stage: &str,
        err: &anyhow::Error,
        server_id: crate::state::ServerId,
        connection_id: ConnectionId,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
        transfers: &Arc<Mutex<HashMap<TransferId, TftpTransfer>>>,
    ) {
        let log = Log::new(Some(&status_tx));
        // Non-fatal: the LLM failed but the client still gets an ERROR packet
        // (wire fallback) and the transfer is torn down cleanly, so this is WARN.
        log.warn(format!(
            "TFTP LLM error during {} for {}: {} - sending ERROR and ending transfer",
            stage, peer_addr, err
        ));

        let packet = Self::llm_failure_error_packet(err);
        match socket.send_to(&packet, peer_addr).await {
            Ok(sent) => {
                app_state
                    .update_connection_stats(
                        server_id,
                        connection_id,
                        None,
                        Some(sent as u64),
                        None,
                        Some(1),
                    )
                    .await;
                log.trace(format!(
                    "TFTP sent ERROR packet (hex): {}",
                    hex::encode(&packet)
                ));
            }
            Err(send_err) => {
                // Could not deliver even the fallback ERROR: genuinely cannot proceed.
                log.error(format!(
                    "TFTP failed to deliver ERROR packet to {}: {}",
                    peer_addr, send_err
                ));
            }
        }

        transfers.lock().await.remove(&transfer_id);
        app_state
            .close_connection_on_server(server_id, connection_id)
            .await;
    }

    /// Build TFTP ERROR packet
    fn build_error_packet(error_code: u16, message: &str) -> Vec<u8> {
        let mut packet = Vec::with_capacity(4 + message.len() + 1);
        packet.extend_from_slice(&5u16.to_be_bytes()); // Opcode ERROR
        packet.extend_from_slice(&error_code.to_be_bytes());
        packet.extend_from_slice(message.as_bytes());
        packet.push(0); // Null terminator
        packet
    }
}
