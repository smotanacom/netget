//! VNC (Virtual Network Computing) server — RFB 3.8 (RFC 6143) over TCP.
//!
//! Hand-written RFB rather than a crate: the server half of RFB 3.8 is a version exchange, a
//! security-type negotiation, ClientInit/ServerInit and seven fixed-length client messages, and
//! no maintained Rust crate implements the *server* side. See `src/server/vnc/CLAUDE.md`.
//!
//! Rust owns the protocol, the framebuffer and the pixel encoding; the LLM owns what is on
//! screen. Four events are raised from the message loop — `vnc_framebuffer_update_request`,
//! `vnc_key_event`, `vnc_pointer_event` and `vnc_client_cut_text` — and the model answers with
//! structured drawing commands (never pixels), clipboard text, "nothing changed", or a
//! disconnect. `crate::display::DisplayCanvas` rasterises the commands; this module encodes the
//! result as a Raw-encoded rectangle.
//!
//! Security is "None" (type 1) only: any client that connects is admitted. There is no VNC-Auth
//! and no password.

pub mod actions;

use crate::display::{Color, DisplayCanvas, DisplayCommand};
use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::{Log, Sink};
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use crate::state::server::{ConnectionState, ConnectionStatus, ProtocolConnectionInfo};
use actions::{
    VncProtocol, RESULT_CLIPBOARD, RESULT_DISPLAY, RESULT_NO_CHANGE, VNC_CLIENT_CUT_TEXT_EVENT,
    VNC_FRAMEBUFFER_UPDATE_REQUEST_EVENT, VNC_KEY_EVENT, VNC_POINTER_EVENT,
};
use anyhow::{anyhow, Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

/// How long to wait for the first byte from a peer that has only connected.
///
/// RFB is server-speaks-first: this server writes `RFB 003.008\n` and the viewer answers with
/// its own twelve-byte version string from inside its connect path, before any human is
/// involved. Thirty seconds is far longer than that takes and is the same bound the rest of
/// this tree gives a peer that has produced nothing.
const FIRST_BYTE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for further bytes once the peer has sent something.
///
/// Half an hour, and the reason it cannot be short is a property of RFB rather than taste: a
/// viewer that has issued an *incremental* `FramebufferUpdateRequest` is required to say
/// nothing more until the server has an update to send it, so a perfectly healthy client
/// watching an unchanging screen is silent for as long as the screen does not change. Closing
/// that would be the TFTP mistake this project already made once — measuring "idle" on a
/// connection that is in the middle of the protocol's own normal behaviour.
///
/// A bound is still necessary, and here more than elsewhere: this server offers security type
/// `None`, so every connection is unauthenticated by construction. Thirty minutes is longer
/// than any interval an operator leaves a viewer attached across and still turns "hold a socket
/// forever" into "hold it for half an hour".
const IDLE_BETWEEN_MESSAGES_TIMEOUT: Duration = Duration::from_secs(1800);

/// Concurrent connections this server admits.
///
/// [`crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS`]. Each connection carries a
/// framebuffer and may buffer a `ClientCutText` up to [`MAX_CUT_TEXT_LEN`], so this is the
/// multiplier that turns those per-connection bounds into total ones.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] is told before the socket closes: nothing.
///
/// RFB *has* a refusal — a security-type count of zero followed by a reason string — but it is
/// not reachable here. It may only be sent after the peer has returned its own ProtocolVersion,
/// and a capped connection is refused before a single byte has been read. Writing
/// `RFB 003.008\n` and then hanging up would be worse than silence: it starts a handshake this
/// server has already decided to decline, and the viewer reports a truncated connection rather
/// than a refused one. So the peer gets a clean EOF and the refusal is recorded where it can
/// carry a reason — `accept_bounded` logs it at WARN with
/// `decision=fail_closed_connection_cap`.
const CONNECTION_CAP_REFUSAL: &[u8] = b"";

/// The read half of a VNC connection, with both deadlines applied to every read.
///
/// [`crate::server::accept_bounded::IdleTimeoutReader`] rather than a `tokio::time::timeout`
/// around one call, because RFB reads are scattered: the version exchange, the security choice,
/// `ClientInit`, each message-type octet and each message body are separate reads, and a peer
/// that sends a message type and then stalls mid-body would otherwise be unbounded. Its
/// deadline is armed lazily — only while a read is actually pending — so the model round-trip
/// inside a message handler, and a `manual` rule parking an event for a human, run with no
/// clock against them at all.
type VncReader = crate::server::accept_bounded::IdleTimeoutReader<tokio::io::ReadHalf<TcpStream>>;

/// VNC server whose display contents are decided by the LLM.
pub struct VncServer;

/// RFB protocol version this server speaks.
const RFB_VERSION: &[u8] = b"RFB 003.008\n";

/// Default desktop name announced in ServerInit.
const DEFAULT_DESKTOP_NAME: &str = "NetGet VNC";

/// Largest ClientCutText payload accepted, in bytes.
///
/// The length field is a client-controlled u32 and the buffer for it is allocated before a
/// single byte of payload is read; without a cap a nine-byte message asks for a 4 GiB
/// allocation.
const MAX_CUT_TEXT_LEN: u32 = 1 << 20;

/// Background of the placeholder screen.
///
/// The placeholder is what a client sees when the model gave no usable answer — an LLM error, a
/// batch in which every action failed, or an empty action list. It is deliberately *not* the
/// same as any screen the model can ask for by accident, and it is deliberately not silence:
/// RFB has no way to say "no answer", so a client left waiting on a full update request simply
/// hangs. `tests/server/vnc/test.rs` asserts this exact colour on the no-answer path.
pub const PLACEHOLDER_BACKGROUND: Color = Color::rgb(24, 26, 34);

/// Foreground of the placeholder screen's diagnostic line.
const PLACEHOLDER_FOREGROUND: Color = Color::rgb(203, 213, 225);

/// Security types offered in the handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum SecurityType {
    None = 1,
}

/// VNC pixel format
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VncPixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_color: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl VncPixelFormat {
    /// The only format this server sends: 32bpp little-endian BGRX.
    pub fn default_rgb888() -> Self {
        Self {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_color: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }
}

/// What the model said in answer to one event.
///
/// The three outcomes are kept apart on purpose. `redraw` is a decision to change the screen,
/// `no_change` is an explicit decision to leave it alone, and `no_answer` is the model failing
/// to say anything usable. Collapsing the last two — treating silence as "nothing changed" —
/// is the fail-open shape this codebase has been bitten by before: a viewer would sit on a
/// blank window with no indication that the model never answered.
#[derive(Default)]
struct Decision {
    redraw: Option<Vec<DisplayCommand>>,
    clipboard: Vec<String>,
    no_change: bool,
    close: bool,
    /// Why no usable answer arrived, if none did.
    no_answer: Option<String>,
}

/// Per-connection state for one VNC client.
struct VncConnection {
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    width: u16,
    height: u16,
    app_state: Arc<AppState>,
    llm_client: OllamaClient,
    protocol: VncProtocol,
    status_tx: mpsc::UnboundedSender<String>,
    write_half: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<TcpStream>>>,
    /// Most recently rendered frame, BGRX, `width * height * 4` bytes.
    last_frame: Option<Vec<u8>>,
    /// True when `last_frame` has changed since the client last received it.
    dirty: bool,
    /// True while the client has a FramebufferUpdateRequest this server has not answered.
    pending_request: bool,
    /// False until the first framebuffer request has been answered.
    answered_first_request: bool,
    /// Button mask carried by the previous PointerEvent, to detect press/release.
    last_button_mask: u8,
}

impl VncServer {
    /// Spawn the VNC server.
    ///
    /// `width`/`height` are validated by `actions::validate_framebuffer_size` before this is
    /// called, so they are within `MIN_FRAMEBUFFER_DIMENSION..=MAX_FRAMEBUFFER_DIMENSION`.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        width: u16,
        height: u16,
        desktop_name: Option<String>,
    ) -> Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;
        let desktop_name =
            Arc::new(desktop_name.unwrap_or_else(|| DEFAULT_DESKTOP_NAME.to_string()));

        // Keep the address at the end of this line: the E2E harness parses the port out of
        // everything after the last "on ".
        Log::new(Some(&status_tx)).info(format!("VNC server listening on {}", local_addr));
        Log::new(Some(&status_tx)).debug(format!(
            "VNC framebuffer {}x{}, desktop name '{}'",
            width, height, desktop_name
        ));

        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "VNC",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((stream, remote_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        let state_clone = app_state.clone();
                        let status_clone = status_tx.clone();
                        let llm_clone = llm_client.clone();
                        let name_clone = desktop_name.clone();

                        Log::new(Some(&status_clone))
                            .info(format!("VNC client connected from {}", remote_addr));

                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Held for the life of the connection: releasing it here would
                                // cap the accept rate rather than the number of live
                                // connections.
                                let _permit = permit;
                                if let Err(e) = Self::handle_connection(
                                    stream,
                                    connection_id,
                                    remote_addr,
                                    local_addr_conn,
                                    server_id,
                                    state_clone,
                                    status_clone,
                                    llm_clone,
                                    width,
                                    height,
                                    &name_clone,
                                )
                                .await
                                {
                                    error!("VNC connection error: {}", e);
                                }
                            })
                            .await;
                    }
                    Err(e) => {
                        // Break rather than continue: a persistent accept error (EMFILE, the
                        // socket being closed under us) previously spun this loop at full CPU
                        // forever, logging on every iteration.
                        Log::new(Some(&status_tx))
                            .error(format!("VNC accept failed, stopping loop: {}", e));
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

    /// Handle a single VNC connection: handshake, init, then the message loop.
    #[allow(clippy::too_many_arguments)]
    async fn handle_connection(
        stream: TcpStream,
        connection_id: ConnectionId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        server_id: crate::state::ServerId,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        llm_client: OllamaClient,
        width: u16,
        height: u16,
        desktop_name: &str,
    ) -> Result<()> {
        let (read_half, write_half) = tokio::io::split(stream);
        // Every read below goes through the deadline pair; nothing else does.
        let mut read_half: VncReader = crate::server::accept_bounded::IdleTimeoutReader::with_first(
            read_half,
            FIRST_BYTE_READ_TIMEOUT,
            IDLE_BETWEEN_MESSAGES_TIMEOUT,
        );
        let write_half = Arc::new(tokio::sync::Mutex::new(write_half));

        let now = crate::utils::clock::Instant::now();
        let conn_state = ConnectionState {
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
        };
        app_state
            .add_connection_to_server(server_id, conn_state)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Peer messaging: the dashboard's "message this peer" / "disconnect this peer" inject
        // actions into THIS connection through the same executor the LLM path uses. Registered
        // before the handshake so the connection is reachable the instant it appears in the rail.
        //
        // Only `vnc_disconnect_client` (which returns `ActionResult::CloseConnection`) is
        // executed by the generic peer task: it half-closes the write side and the message loop
        // reads EOF. The drawing verbs (`vnc_render_display`, `vnc_set_clipboard`) return
        // `ActionResult::Custom` and are reported as "executed" without touching the wire,
        // because rendering needs this connection's framebuffer state, which lives in the
        // message loop. See `src/server/vnc/CLAUDE.md`.
        let peer_protocol: Arc<dyn crate::llm::actions::protocol_trait::Server> =
            Arc::new(VncProtocol::new());
        let peer_rx = crate::server::peer_support::register_peer_channel(
            &app_state,
            server_id,
            connection_id.as_u32(),
        )
        .await;
        crate::server::peer_support::spawn_peer_command_task(
            peer_rx,
            peer_protocol,
            app_state.clone(),
            server_id,
            connection_id.as_u32(),
            write_half.clone(),
            status_tx.clone(),
        );

        let result = async {
            Self::perform_handshake(&mut read_half, &write_half, &status_tx).await?;

            Log::new(Some(&status_tx)).debug(format!("VNC handshake complete for {}", remote_addr));
            app_state
                .update_vnc_connection_auth(server_id, connection_id, true, None)
                .await;

            Self::handle_client_init(
                &mut read_half,
                &write_half,
                &status_tx,
                width,
                height,
                desktop_name,
            )
            .await?;

            let mut connection = VncConnection {
                connection_id,
                server_id,
                width,
                height,
                app_state: app_state.clone(),
                llm_client,
                protocol: VncProtocol::new(),
                status_tx: status_tx.clone(),
                write_half: write_half.clone(),
                last_frame: None,
                dirty: false,
                pending_request: false,
                answered_first_request: false,
                last_button_mask: 0,
            };
            connection.message_loop(read_half).await
        }
        .await;

        // Always drop the connection from the server view, whatever went wrong above; the
        // TUI otherwise shows a connection that no longer exists. Remove the peer handle on the
        // same exit path (EOF, read error, unknown message, injected disconnect) so the rail
        // stops offering "message this peer" for a connection that is gone; the peer task also
        // removes it on an injected disconnect, and both calls are idempotent.
        app_state
            .remove_peer_handle(server_id, connection_id.as_u32())
            .await;
        app_state
            .remove_connection_from_server(server_id, connection_id)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        result
    }

    /// RFB 3.8 handshake: ProtocolVersion, security types, SecurityResult.
    async fn perform_handshake(
        read_half: &mut VncReader,
        write_half: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<TcpStream>>>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        {
            let mut writer = write_half.lock().await;
            writer.write_all(RFB_VERSION).await?;
            writer.flush().await?;
        }
        trace!(
            "Sent RFB version: {}",
            String::from_utf8_lossy(RFB_VERSION).trim()
        );

        let mut client_version = [0u8; 12];
        read_half.read_exact(&mut client_version).await?;
        trace!(
            "Received client version: {}",
            String::from_utf8_lossy(&client_version).trim()
        );

        {
            let mut writer = write_half.lock().await;
            writer.write_u8(1).await?; // Number of security types
            writer.write_u8(SecurityType::None as u8).await?;
            writer.flush().await?;
        }
        trace!("Sent security types: [None]");

        let chosen_security = read_half.read_u8().await?;
        trace!("Client chose security type: {}", chosen_security);

        if chosen_security == SecurityType::None as u8 {
            let mut writer = write_half.lock().await;
            writer.write_u32(0).await?; // 0 = OK
            writer.flush().await?;
            trace!("Sent SecurityResult: OK");
            Log::new(Some(status_tx)).debug("VNC client authenticated");
            Ok(())
        } else {
            // RFB 3.8 requires a reason string after a failed SecurityResult, and that string
            // is rendered verbatim by the peer's viewer. It is a byte literal for exactly that
            // reason: nothing from NetGet's own diagnostics may reach it.
            let reason = b"only security type 1 (None) is supported";
            {
                let mut writer = write_half.lock().await;
                writer.write_u32(1).await?; // 1 = Failed
                writer.write_u32(reason.len() as u32).await?;
                writer.write_all(reason).await?;
                writer.flush().await?;
            }
            // The protocol refused this peer, not the model — no model was consulted. Tagged
            // so it is never read as a fail-closed answer to an event.
            Log::new(Some(status_tx)).warn(format!(
                "VNC handshake refused decision=protocol_error: client requested unsupported \
                 security type {}",
                chosen_security
            ));
            Err(anyhow!(
                "client requested unsupported security type {chosen_security}"
            ))
        }
    }

    /// ClientInit (shared flag) then ServerInit (geometry, pixel format, desktop name).
    async fn handle_client_init(
        read_half: &mut VncReader,
        write_half: &Arc<tokio::sync::Mutex<tokio::io::WriteHalf<TcpStream>>>,
        status_tx: &mpsc::UnboundedSender<String>,
        width: u16,
        height: u16,
        desktop_name: &str,
    ) -> Result<()> {
        let shared_flag = read_half.read_u8().await?;
        trace!("Client shared flag: {}", shared_flag);

        let pixel_format = VncPixelFormat::default_rgb888();
        let name = desktop_name.as_bytes();

        let mut message = Vec::with_capacity(24 + name.len());
        message.extend_from_slice(&width.to_be_bytes());
        message.extend_from_slice(&height.to_be_bytes());
        message.push(pixel_format.bits_per_pixel);
        message.push(pixel_format.depth);
        message.push(u8::from(pixel_format.big_endian));
        message.push(u8::from(pixel_format.true_color));
        message.extend_from_slice(&pixel_format.red_max.to_be_bytes());
        message.extend_from_slice(&pixel_format.green_max.to_be_bytes());
        message.extend_from_slice(&pixel_format.blue_max.to_be_bytes());
        message.push(pixel_format.red_shift);
        message.push(pixel_format.green_shift);
        message.push(pixel_format.blue_shift);
        message.extend_from_slice(&[0, 0, 0]); // Padding
        message.extend_from_slice(&(name.len() as u32).to_be_bytes());
        message.extend_from_slice(name);

        {
            let mut writer = write_half.lock().await;
            writer.write_all(&message).await?;
            writer.flush().await?;
        }

        Log::new(Some(status_tx)).debug(format!(
            "VNC initialized: {}x{} framebuffer '{}'",
            width, height, desktop_name
        ));

        Ok(())
    }
}

impl VncConnection {
    /// Main client-message loop.
    ///
    /// One connection is handled strictly sequentially: a model call for one message finishes
    /// before the next message is read. RFB client messages are small and the socket buffers
    /// them, so nothing is lost; the cost is latency on input that arrives during a call.
    async fn message_loop(&mut self, mut read_half: VncReader) -> Result<()> {
        loop {
            let message_type = match read_half.read_u8().await {
                Ok(t) => t,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    Log::new(Some(&self.status_tx)).info("VNC client disconnected");
                    break;
                }
                Err(e) => return Err(e.into()),
            };

            trace!("Received message type: {}", message_type);

            // Bytes consumed for this message, starting with the type byte already read. Fed to
            // `update_connection_stats` so the rail's `↓` counter and `last_activity` are live.
            let mut bytes_in: u64 = 1;

            let keep_going = match message_type {
                0 => {
                    // SetPixelFormat. Read and discarded: the server always sends 32bpp BGRX,
                    // so a client that requests anything else will render garbage.
                    let mut buf = [0u8; 19]; // 3 padding + 16 pixel format
                    read_half.read_exact(&mut buf).await?;
                    bytes_in += 19;
                    trace!("SetPixelFormat received (ignored, server always sends 32bpp BGRX)");
                    true
                }
                2 => {
                    // SetEncodings. Raw is the only encoding this server produces, and RFB
                    // requires every client to support it, so the list is parsed for framing
                    // and discarded.
                    let _padding = read_half.read_u8().await?;
                    let num_encodings = read_half.read_u16().await?;
                    for _ in 0..num_encodings {
                        let _ = read_half.read_i32().await?;
                    }
                    bytes_in += 3 + 4 * num_encodings as u64;
                    trace!("SetEncodings received: {} encodings", num_encodings);
                    true
                }
                3 => {
                    let incremental = read_half.read_u8().await? != 0;
                    let x = read_half.read_u16().await?;
                    let y = read_half.read_u16().await?;
                    let width = read_half.read_u16().await?;
                    let height = read_half.read_u16().await?;
                    bytes_in += 9;
                    trace!(
                        "FramebufferUpdateRequest: incremental={}, x={}, y={}, w={}, h={}",
                        incremental,
                        x,
                        y,
                        width,
                        height
                    );
                    self.handle_update_request(incremental).await?
                }
                4 => {
                    let down = read_half.read_u8().await? != 0;
                    let _padding = read_half.read_u16().await?;
                    let keysym = read_half.read_u32().await?;
                    bytes_in += 7;
                    self.handle_key_event(down, keysym).await?
                }
                5 => {
                    let button_mask = read_half.read_u8().await?;
                    let x = read_half.read_u16().await?;
                    let y = read_half.read_u16().await?;
                    bytes_in += 5;
                    self.handle_pointer_event(button_mask, x, y).await?
                }
                6 => {
                    let _padding = read_half.read_u8().await?;
                    let _padding = read_half.read_u16().await?;
                    let length = read_half.read_u32().await?;
                    bytes_in += 7;
                    if length > MAX_CUT_TEXT_LEN {
                        // The buffer used to be allocated straight from this client-controlled
                        // u32, so a nine-byte message could ask for a 4 GiB allocation.
                        Log::new(Some(&self.status_tx)).warn(format!(
                            "VNC ClientCutText length {} exceeds {} byte cap, closing connection",
                            length, MAX_CUT_TEXT_LEN
                        ));
                        false
                    } else {
                        let mut text = vec![0u8; length as usize];
                        read_half.read_exact(&mut text).await?;
                        bytes_in += length as u64;
                        self.handle_cut_text(&text).await?
                    }
                }
                other => {
                    // The message length is unknown, so the stream cannot be resynchronised:
                    // continuing would read this message's body as message types. Close.
                    Log::new(Some(&self.status_tx)).warn(format!(
                        "VNC client sent unknown message type {}, closing connection",
                        other
                    ));
                    false
                }
            };

            self.app_state
                .update_connection_stats(
                    self.server_id,
                    self.connection_id,
                    Some(bytes_in),
                    None,
                    Some(1),
                    None,
                )
                .await;

            if !keep_going {
                break;
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------------
    // Event handling
    // ------------------------------------------------------------------

    /// A FramebufferUpdateRequest arrived.
    ///
    /// Non-incremental requests ("send me the whole screen") consult the model. Incremental
    /// ones ("tell me when something changes") do not: RFB permits the server to hold such a
    /// request open until the screen actually changes, and answering each one with a model
    /// call would spend a round-trip per poll on an idle desktop. A held request is answered
    /// by the next event that redraws.
    async fn handle_update_request(&mut self, incremental: bool) -> Result<bool> {
        self.pending_request = true;

        if incremental && self.answered_first_request {
            if self.dirty {
                self.send_pending_frame(false).await?;
            } else {
                trace!(
                    "VNC incremental request held open for {} (screen unchanged)",
                    self.connection_id
                );
            }
            return Ok(true);
        }

        let event = Event::new(
            &VNC_FRAMEBUFFER_UPDATE_REQUEST_EVENT,
            serde_json::json!({
                "width": self.width,
                "height": self.height,
                "first_request": !self.answered_first_request,
            }),
        );
        let decision = self.consult(&event).await;
        let close = self
            .apply(decision, "the model gave no screen to draw")
            .await?;

        // A full update request must be answered: RFB has no "nothing to send" reply, so a
        // client that asked for the whole screen and gets nothing waits forever.
        self.send_pending_frame(true).await?;
        self.answered_first_request = true;

        Ok(!close)
    }

    /// A key went down or up.
    async fn handle_key_event(&mut self, down: bool, keysym: u32) -> Result<bool> {
        let key_name = keysym_name(keysym);
        // FileOnly: the vnc_key_event template renders the equivalent line to the TUI.
        Log::new(Some(&self.status_tx)).debug(format!(
            "VNC KeyEvent: down={}, key={}, keysym={}",
            down, key_name, keysym
        ));

        let event = Event::new(
            &VNC_KEY_EVENT,
            serde_json::json!({
                "down": down,
                "keysym": keysym,
                "key": key_name,
            }),
        );
        let decision = self.consult(&event).await;
        let close = self
            .apply(decision, "the model gave no screen to draw for this key")
            .await?;
        self.send_pending_frame(false).await?;
        Ok(!close)
    }

    /// A pointer message arrived.
    ///
    /// Only a change in the button mask raises `vnc_pointer_event`. A viewer emits a
    /// PointerEvent for every pixel of mouse travel, and one model round-trip per pixel is not
    /// a feature; movement is dual-logged instead so it is still visible in the TUI and over
    /// MCP, which is where somebody watching a session would look.
    async fn handle_pointer_event(&mut self, button_mask: u8, x: u16, y: u16) -> Result<bool> {
        let previous = self.last_button_mask;
        self.last_button_mask = button_mask;

        if button_mask == previous {
            // Movement raises no event, so this DEBUG line is the only record of it:
            // keep it on the TUI (Both) so input is never silently invisible.
            Log::new(Some(&self.status_tx)).debug_to(
                Sink::Both,
                format!(
                    "VNC PointerEvent: move to {},{} (buttons={:#04b})",
                    x, y, button_mask
                ),
            );
            return Ok(true);
        }

        let pressed = button_mask.count_ones() > previous.count_ones();
        // FileOnly: a button change raises vnc_pointer_event, whose template renders
        // the equivalent line to the TUI.
        Log::new(Some(&self.status_tx)).debug(format!(
            "VNC PointerEvent: {} at {},{} (buttons={:#04b})",
            if pressed { "press" } else { "release" },
            x,
            y,
            button_mask
        ));

        let event = Event::new(
            &VNC_POINTER_EVENT,
            serde_json::json!({
                "x": x,
                "y": y,
                "pressed": pressed,
                "buttons": button_names(button_mask),
                "button_mask": button_mask,
            }),
        );
        let decision = self.consult(&event).await;
        let close = self
            .apply(decision, "the model gave no screen to draw for this click")
            .await?;
        self.send_pending_frame(false).await?;
        Ok(!close)
    }

    /// The client sent us its clipboard.
    async fn handle_cut_text(&mut self, raw: &[u8]) -> Result<bool> {
        // RFB says ClientCutText is Latin-1. Decoding byte-per-character can never fail, which
        // matters: this is attacker-controlled input and must not be able to abort the task.
        let text: String = raw.iter().map(|&b| b as char).collect();
        // FileOnly: the vnc_client_cut_text template renders the equivalent line to the TUI.
        Log::new(Some(&self.status_tx)).debug(format!(
            "VNC ClientCutText: {} bytes from {}",
            raw.len(),
            self.connection_id
        ));
        trace!("VNC ClientCutText payload: {:?}", text);

        let event = Event::new(
            &VNC_CLIENT_CUT_TEXT_EVENT,
            serde_json::json!({ "text": text }),
        );
        let decision = self.consult(&event).await;
        // Clipboard text needs no screen: an event that only produced clipboard output is a
        // complete answer, so no placeholder is drawn for it.
        let close = self.apply(decision, "").await?;
        self.send_pending_frame(false).await?;
        Ok(!close)
    }

    // ------------------------------------------------------------------
    // LLM plumbing
    // ------------------------------------------------------------------

    /// Ask the event handlers (script → static → LLM) what to do about `event`.
    ///
    /// Never returns an error: a failure here has to leave the connection alive so the caller
    /// can put a placeholder on screen. The reason is carried in `Decision::no_answer`.
    async fn consult(&self, event: &Event) -> Decision {
        let mut decision = Decision::default();
        let log = Log::new(Some(&self.status_tx));

        let execution = match call_llm(
            &self.llm_client,
            &self.app_state,
            self.server_id,
            Some(self.connection_id),
            event,
            &self.protocol,
        )
        .await
        {
            Ok(execution) => execution,
            Err(e) => {
                // The backend failed. The peer gets the placeholder screen with a fixed
                // caption; the error text stays here. `fail_closed_llm_overloaded` and
                // `fail_closed_llm_error` are kept apart so a saturated backend and a dead
                // one are not the same line, and neither can be mistaken for the model
                // having chosen to leave the screen alone (`decision=model_answer` with a
                // `vnc_no_change`).
                let tag = match crate::utils::wire_failure::WireFailure::classify(&e) {
                    crate::utils::wire_failure::WireFailure::Overloaded => {
                        "fail_closed_llm_overloaded"
                    }
                    crate::utils::wire_failure::WireFailure::Unavailable => "fail_closed_llm_error",
                };
                log.error(format!(
                    "VNC {} on {} not answered decision={}: {}",
                    event.event_type.id, self.connection_id, tag, e
                ));
                decision.no_answer = Some(e.to_string());
                return decision;
            }
        };

        for message in execution.messages {
            let _ = self.status_tx.send(message);
        }

        // Flatten `Multiple` so a batched answer is not silently ignored.
        let mut flattened = Vec::with_capacity(execution.protocol_results.len());
        flatten_results(execution.protocol_results, &mut flattened);

        let mut saw_result = false;
        for result in flattened {
            match result {
                ActionResult::Custom { name, data } if name == RESULT_DISPLAY => {
                    saw_result = true;
                    match serde_json::from_value::<Vec<DisplayCommand>>(
                        data.get("commands").cloned().unwrap_or_default(),
                    ) {
                        Ok(commands) => decision.redraw = Some(commands),
                        Err(e) => {
                            // The action executor already validated this JSON, so reaching
                            // here means the two disagree — a bug worth shouting about.
                            error!("VNC render result could not be decoded: {}", e);
                            decision.no_answer =
                                Some(format!("render result could not be decoded: {e}"));
                        }
                    }
                }
                ActionResult::Custom { name, .. } if name == RESULT_NO_CHANGE => {
                    saw_result = true;
                    decision.no_change = true;
                }
                ActionResult::Custom { name, data } if name == RESULT_CLIPBOARD => {
                    saw_result = true;
                    if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
                        decision.clipboard.push(text.to_string());
                    }
                }
                ActionResult::CloseConnection => {
                    saw_result = true;
                    decision.close = true;
                }
                ActionResult::NoAction => {}
                other => {
                    warn!("VNC ignoring unexpected action result: {:?}", other);
                }
            }
        }

        // Exactly one `decision=` line per event, whatever happened. Without it a model that
        // refused, a model that said nothing, and an executor that rejected the model's own
        // action are one indistinguishable "the screen did not change".
        if decision.no_answer.is_some() {
            // Only the render-decode branch above reaches here: the executor accepted the
            // action and this module could not read its result, which is a bug in the pair.
            log.error(format!(
                "VNC {} on {} decision=fail_closed_bad_action: the render result could not be \
                 decoded",
                event.event_type.id, self.connection_id
            ));
        } else if !saw_result {
            let detail = if execution.failures.is_empty() {
                "no protocol action was returned".to_string()
            } else {
                execution
                    .failures
                    .iter()
                    .map(|f| format!("{}: {}", f.action, f.error))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            // An empty answer is the model's own silence (WARN); an answer whose actions the
            // executor refused is a malformed answer (ERROR). Both draw the placeholder.
            if execution.failures.is_empty() {
                log.warn(format!(
                    "VNC {} on {} produced no usable action decision=model_silent ({})",
                    event.event_type.id, self.connection_id, detail
                ));
            } else {
                log.error(format!(
                    "VNC {} on {} produced no usable action decision=fail_closed_bad_action ({})",
                    event.event_type.id, self.connection_id, detail
                ));
            }
            decision.no_answer = Some(detail);
        } else if decision.close {
            log.info(format!(
                "VNC {} on {} decision=model_reject (the model closed the connection)",
                event.event_type.id, self.connection_id
            ));
        } else {
            log.info(format!(
                "VNC {} on {} decision=model_answer",
                event.event_type.id, self.connection_id
            ));
        }

        decision
    }

    /// Apply a decision: render, push clipboard text, or draw the placeholder.
    ///
    /// Returns true when the connection should close. `no_answer_reason` empty means this event
    /// does not need a screen, so a missing render is not a failure.
    async fn apply(&mut self, decision: Decision, no_answer_reason: &str) -> Result<bool> {
        for text in &decision.clipboard {
            self.send_server_cut_text(text).await?;
        }

        if let Some(commands) = decision.redraw {
            let pixels = render_frame(self.width, self.height, commands).await?;
            self.store_frame(pixels);
        } else if decision.no_change {
            trace!(
                "VNC screen held unchanged for {} by explicit decision",
                self.connection_id
            );
        } else if decision.no_answer.is_some() && !no_answer_reason.is_empty() {
            // Structurally distinct from `vnc_no_change`: the model saying "leave the screen
            // alone" keeps whatever is there, while no answer at all puts a placeholder up
            // that says so.
            let pixels = render_frame(
                self.width,
                self.height,
                placeholder_commands(self.height, no_answer_reason),
            )
            .await?;
            self.store_frame(pixels);
        }

        Ok(decision.close)
    }

    /// Record a freshly rendered frame as the current screen.
    fn store_frame(&mut self, pixels: Vec<u8>) {
        self.last_frame = Some(pixels);
        self.dirty = true;
    }

    /// Answer an outstanding FramebufferUpdateRequest, if there is one.
    ///
    /// `force` answers even when nothing changed, which a non-incremental request requires.
    /// Otherwise an unchanged screen leaves the request open, exactly as a real VNC server
    /// does, and it is answered by the next redraw.
    async fn send_pending_frame(&mut self, force: bool) -> Result<()> {
        if !self.pending_request || !(force || self.dirty) {
            return Ok(());
        }

        if self.last_frame.is_none() {
            // Nothing has ever been drawn and the client is waiting on a full update.
            let pixels = render_frame(
                self.width,
                self.height,
                placeholder_commands(self.height, "no screen has been drawn yet"),
            )
            .await?;
            self.store_frame(pixels);
        }

        let Some(pixels) = self.last_frame.as_ref() else {
            return Ok(());
        };

        let mut frame = framebuffer_update_header(self.width, self.height);
        frame.extend_from_slice(pixels);

        let frame_len = frame.len();
        {
            let mut writer = self.write_half.lock().await;
            writer.write_all(&frame).await?;
            writer.flush().await?;
        }
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                None,
                Some(frame_len as u64),
                None,
                Some(1),
            )
            .await;

        self.pending_request = false;
        self.dirty = false;
        // Send summary FileOnly: the vnc_render_display action template already reports
        // the render to the TUI.
        Log::new(Some(&self.status_tx)).debug(format!(
            "VNC sent {}x{} framebuffer update ({} bytes) to {}",
            self.width,
            self.height,
            frame.len(),
            self.connection_id
        ));
        Ok(())
    }

    /// Push text to the client's clipboard (RFB ServerCutText, message type 3).
    async fn send_server_cut_text(&self, text: &str) -> Result<()> {
        // RFB ServerCutText is Latin-1; anything outside it has no representation on the wire.
        let latin1: Vec<u8> = text
            .chars()
            .map(|c| if (c as u32) <= 0xFF { c as u8 } else { b'?' })
            .collect();

        let mut message = Vec::with_capacity(8 + latin1.len());
        message.push(3); // ServerCutText
        message.extend_from_slice(&[0, 0, 0]); // Padding
        message.extend_from_slice(&(latin1.len() as u32).to_be_bytes());
        message.extend_from_slice(&latin1);

        let message_len = message.len();
        {
            let mut writer = self.write_half.lock().await;
            writer.write_all(&message).await?;
            writer.flush().await?;
        }
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                None,
                Some(message_len as u64),
                None,
                Some(1),
            )
            .await;
        // Send summary FileOnly: the vnc_set_clipboard action template already reports
        // the send to the TUI.
        Log::new(Some(&self.status_tx)).debug(format!(
            "VNC sent {} bytes of clipboard text to {}",
            latin1.len(),
            self.connection_id
        ));
        Ok(())
    }
}

/// Flatten nested [`ActionResult::Multiple`] into a single ordered list.
fn flatten_results(results: Vec<ActionResult>, out: &mut Vec<ActionResult>) {
    for result in results {
        match result {
            ActionResult::Multiple(inner) => flatten_results(inner, out),
            other => out.push(other),
        }
    }
}

/// The RFB FramebufferUpdate header for a single full-framebuffer Raw rectangle.
///
/// The client's requested region is deliberately not echoed back: RFB lets the server answer
/// with any rectangle, and answering with the client's extent (as this code used to) both
/// contradicts the size announced in ServerInit and lets a client ask for 65535x65535.
fn framebuffer_update_header(width: u16, height: u16) -> Vec<u8> {
    let mut header = Vec::with_capacity(16);
    header.push(0); // Message type: FramebufferUpdate
    header.push(0); // Padding
    header.extend_from_slice(&1u16.to_be_bytes()); // Number of rectangles
    header.extend_from_slice(&0u16.to_be_bytes()); // X position
    header.extend_from_slice(&0u16.to_be_bytes()); // Y position
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&0i32.to_be_bytes()); // Encoding: Raw
    header
}

/// Rasterise display commands into the BGRX pixel payload of a Raw rectangle.
///
/// Rendering is CPU-bound (tiny-skia, plus font shaping on the first text draw), so it runs on
/// the blocking pool rather than stalling a tokio worker. A panic inside the canvas surfaces
/// here as an error instead of silently killing the connection task.
async fn render_frame(width: u16, height: u16, commands: Vec<DisplayCommand>) -> Result<Vec<u8>> {
    let (w, h) = (width as u32, height as u32);
    let pixels = tokio::task::spawn_blocking(move || {
        let mut canvas = DisplayCanvas::new(w, h);
        canvas.add_commands(commands);
        let image = canvas.render();
        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for pixel in image.pixels() {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0]); // BGRX
        }
        out
    })
    .await
    .context("VNC framebuffer rendering failed")?;

    debug!("Rendered VNC framebuffer: {}x{}", width, height);
    Ok(pixels)
}

/// The screen shown when no usable answer arrived.
fn placeholder_commands(height: u16, reason: &str) -> Vec<DisplayCommand> {
    vec![
        DisplayCommand::SetBackground {
            color: PLACEHOLDER_BACKGROUND,
        },
        DisplayCommand::DrawText {
            x: 24,
            y: (height / 2) as u32,
            text: format!("NetGet VNC: {reason}"),
            font_size: 18,
            color: PLACEHOLDER_FOREGROUND,
        },
    ]
}

/// Names of the buttons held in an RFB button mask.
fn button_names(mask: u8) -> Vec<&'static str> {
    const NAMES: [&str; 5] = ["left", "middle", "right", "scroll_up", "scroll_down"];
    NAMES
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Human-readable name for an X11 keysym, so the model never has to decode one.
fn keysym_name(keysym: u32) -> String {
    // Printable ASCII and Latin-1 keysyms are the character itself.
    if (0x20..=0x7E).contains(&keysym) || (0xA0..=0xFF).contains(&keysym) {
        if let Some(c) = char::from_u32(keysym) {
            return c.to_string();
        }
    }
    // Unicode keysyms: 0x01000000 + code point.
    if (0x01000100..=0x0110FFFF).contains(&keysym) {
        if let Some(c) = char::from_u32(keysym - 0x01000000) {
            return c.to_string();
        }
    }
    // Function keys F1-F35 are contiguous from 0xFFBE.
    if (0xFFBE..=0xFFE0).contains(&keysym) {
        return format!("F{}", keysym - 0xFFBD);
    }
    match keysym {
        0xFF08 => "BackSpace",
        0xFF09 => "Tab",
        0xFF0A => "Linefeed",
        0xFF0B => "Clear",
        0xFF0D => "Return",
        0xFF13 => "Pause",
        0xFF14 => "Scroll_Lock",
        0xFF15 => "Sys_Req",
        0xFF1B => "Escape",
        0xFF50 => "Home",
        0xFF51 => "Left",
        0xFF52 => "Up",
        0xFF53 => "Right",
        0xFF54 => "Down",
        0xFF55 => "Page_Up",
        0xFF56 => "Page_Down",
        0xFF57 => "End",
        0xFF58 => "Begin",
        0xFF63 => "Insert",
        0xFF7F => "Num_Lock",
        0xFF8D => "KP_Enter",
        0xFFE1 => "Shift_L",
        0xFFE2 => "Shift_R",
        0xFFE3 => "Control_L",
        0xFFE4 => "Control_R",
        0xFFE5 => "Caps_Lock",
        0xFFE7 => "Meta_L",
        0xFFE8 => "Meta_R",
        0xFFE9 => "Alt_L",
        0xFFEA => "Alt_R",
        0xFFEB => "Super_L",
        0xFFEC => "Super_R",
        0xFFFF => "Delete",
        _ => return format!("keysym_{keysym:#x}"),
    }
    .to_string()
}
