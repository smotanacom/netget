//! DICT (RFC 2229) server — the model is the dictionary.
//!
//! The server greets with `220`, then reads one command per line. The commands whose answer is
//! *content* — DEFINE, MATCH, SHOW DB/STRAT/INFO/SERVER — raise an event and the model answers
//! with a structured action. Everything whose answer is fixed by the protocol — CLIENT, STATUS,
//! HELP, OPTION, QUIT, AUTH, and any command this server does not know — is answered here
//! without consulting anyone, so a stranger typing garbage costs no model call.
//!
//! Four properties worth knowing before changing the loop:
//!
//! 1. **A command is a line, and a line is bounded.** RFC 2229 caps a command line at 1024
//!    bytes including CRLF ([`wire::MAX_LINE_BYTES`]). A longer one is refused with `500` and
//!    the connection closes — there is no way to resynchronise with a peer that ignored a
//!    MUST, and buffering toward its newline is a memory hole.
//! 2. **The model cannot write framing.** Its actions are rendered by [`wire`], and the loop
//!    then checks that the reply's status code fits the command it answers (a `152` to a
//!    DEFINE is refused, not sent). A reply that does not fit, no reply, and a backend failure
//!    all end the same way: `420 Server temporarily unavailable`, then close.
//! 3. **OPTION MIME is connection state the executor cannot see.** The action executor is
//!    stateless, so the loop applies [`wire::apply_mime`] to what the executor rendered.
//!    Replies injected from the dashboard (`[ message ]`) do not pass through the loop and are
//!    written without the MIME preface.
//! 4. **The deadlines wrap the read, not the answer.** A command parked for a human under a
//!    `manual` rule is never closed by `first_byte_timeout_secs` or `idle_timeout_secs`.
pub mod actions;
pub mod wire;

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::{Event, EventType};
use crate::server::connection::ConnectionId;
use crate::state::app_state::AppState;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};

pub use wire::MAX_LINE_BYTES;

/// How long a peer that has been greeted may take to send its first command.
///
/// DICT is server-speaks-first, so a real client answers the banner at once (`dict(1)` sends
/// `CLIENT` within the same millisecond). 300 seconds is not about real clients: it is the
/// window a `manual` rule gives a human, and the peer may be NetGet's own TCP client whose
/// banner event is parked for its operator. A listener exposed to strangers should lower it
/// through `first_byte_timeout_secs`.
const FIRST_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the server waits for the next command after answering one.
///
/// A DICT session is several lookups down one connection when a person drives it by hand, and
/// `dict(1)` sends QUIT the moment it has its answer, so this bound only ever matters for a peer
/// that has gone quiet. `idle_timeout_secs` overrides it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Concurrent connections admitted before new ones are refused — the house default. A DICT
/// connection holds a [`MAX_LINE_BYTES`] buffer and one task, and each deadline above is 300
/// seconds, so without a cap one peer could hold an unbounded number of them.
const MAX_CONNECTIONS: usize = crate::server::accept_bounded::DEFAULT_MAX_CONNECTIONS;

/// What a peer over [`MAX_CONNECTIONS`] is told: RFC 2229 §3.1 lists `420 Server temporarily
/// unavailable` as a greeting the server may send instead of `220`, which is exactly this.
const CONNECTION_CAP_REFUSAL: &[u8] = b"420 Server temporarily unavailable\r\n";

/// The peer-visible answer when the model could not or did not answer. Both categories are
/// `420` — DICT has no separate "overloaded" code — and the text says which, from fixed
/// literals, so nothing derived from the error can reach the wire.
const UNAVAILABLE_REPLY: &[u8] = b"420 Server temporarily unavailable\r\n";
const OVERLOADED_REPLY: &[u8] = b"420 Server temporarily unavailable, backend at capacity\r\n";

/// Fixed HELP text (113). It lists what this server implements, which only NetGet knows.
const HELP_LINES: &[&str] = &[
    "DEFINE database word         -- look up word in database",
    "MATCH database strategy word -- match word in database using strategy",
    "SHOW DB                      -- list all accessible databases",
    "SHOW DATABASES               -- list all accessible databases",
    "SHOW STRAT                   -- list available matching strategies",
    "SHOW STRATEGIES              -- list available matching strategies",
    "SHOW INFO database           -- provide information about the database",
    "SHOW SERVER                  -- provide site-specific information",
    "OPTION MIME                  -- use MIME headers",
    "CLIENT info                  -- identify client to server",
    "STATUS                       -- display timing information",
    "HELP                         -- display this help information",
    "QUIT                         -- terminate connection",
];

pub struct DictServer;

impl DictServer {
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
        first_byte_timeout_secs: Option<u64>,
        idle_timeout_secs: Option<u64>,
    ) -> Result<SocketAddr> {
        let first_timeout = first_byte_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(FIRST_COMMAND_TIMEOUT);
        let idle_timeout = idle_timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(IDLE_TIMEOUT);
        let listener = TcpListener::bind(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("DICT server listening on {}", local_addr));

        let protocol = Arc::new(actions::DictProtocol::new());
        let task_registrar = app_state.clone();
        let limiter = crate::server::accept_bounded::ConnectionLimiter::new(MAX_CONNECTIONS);
        let accept_handle = tokio::spawn(async move {
            loop {
                match crate::server::accept_bounded::accept_bounded(
                    &listener,
                    &limiter,
                    CONNECTION_CAP_REFUSAL,
                    "DICT",
                    Some(&status_tx),
                )
                .await
                {
                    Ok((socket, peer_addr, permit)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = crate::utils::clock::Instant::now();
                        app_state
                            .add_connection_to_server(
                                server_id,
                                ServerConnectionState {
                                    id: connection_id,
                                    remote_addr: peer_addr,
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
                        Log::new(Some(&status_tx))
                            .info(format!("DICT client connected from {}", peer_addr));

                        let session = Session {
                            peer_addr,
                            llm_client: llm_client.clone(),
                            app_state: app_state.clone(),
                            status_tx: status_tx.clone(),
                            server_id,
                            protocol: protocol.clone(),
                            connection_id,
                            first_timeout,
                            idle_timeout,
                        };
                        // Tracked, not detached: stop_server must abort this task too.
                        let task_owner = app_state.clone();
                        task_owner
                            .spawn_server_task(server_id, async move {
                                // Released when the connection ends; this task is the whole
                                // connection, so MAX_CONNECTIONS caps live connections.
                                let _permit = permit;
                                session.run(socket).await
                            })
                            .await;
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx)).error(format!("DICT accept error: {}", e));
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

/// One parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Define {
        database: String,
        word: String,
    },
    Match {
        database: String,
        strategy: String,
        word: String,
    },
    ShowDatabases,
    ShowStrategies,
    ShowInfo {
        database: String,
    },
    ShowServer,
    Client,
    Status,
    Help,
    OptionMime,
    /// `OPTION` with anything other than MIME.
    OptionOther,
    Quit,
    /// AUTH, SASLAUTH, SASLRESP: recognised, not implemented.
    NotImplemented,
    /// A known command with the wrong parameters.
    BadParameters,
    /// Not a DICT command.
    Unknown,
}

/// Parse one command line (without its line terminator). Commands are case-insensitive.
pub fn parse_command(line: &str) -> Command {
    let Ok(args) = wire::split_args(line) else {
        return Command::BadParameters;
    };
    let Some(verb) = args.first() else {
        return Command::Unknown;
    };
    let rest = &args[1..];
    match verb.to_ascii_uppercase().as_str() {
        "DEFINE" => match rest {
            [database, word] => Command::Define {
                database: database.clone(),
                word: word.clone(),
            },
            _ => Command::BadParameters,
        },
        "MATCH" => match rest {
            [database, strategy, word] => Command::Match {
                database: database.clone(),
                strategy: strategy.clone(),
                word: word.clone(),
            },
            _ => Command::BadParameters,
        },
        "SHOW" => {
            let sub = rest.first().map(|s| s.to_ascii_uppercase());
            match (sub.as_deref(), rest.len()) {
                (Some("DB") | Some("DATABASES"), 1) => Command::ShowDatabases,
                (Some("STRAT") | Some("STRATEGIES"), 1) => Command::ShowStrategies,
                (Some("INFO"), 2) => Command::ShowInfo {
                    database: rest[1].clone(),
                },
                (Some("SERVER"), 1) => Command::ShowServer,
                _ => Command::BadParameters,
            }
        }
        "CLIENT" => Command::Client,
        "STATUS" => Command::Status,
        "HELP" => Command::Help,
        "OPTION" => match rest {
            [opt] if opt.eq_ignore_ascii_case("MIME") => Command::OptionMime,
            [] => Command::BadParameters,
            _ => Command::OptionOther,
        },
        "QUIT" => Command::Quit,
        "AUTH" | "SASLAUTH" | "SASLRESP" => Command::NotImplemented,
        _ => Command::Unknown,
    }
}

impl Command {
    /// The event this command raises and its data, for the commands the model answers.
    fn event(&self) -> Option<(&'static EventType, serde_json::Value)> {
        use actions::{DICT_DEFINE_EVENT, DICT_MATCH_EVENT, DICT_SHOW_EVENT};
        let (event, data) = match self {
            Command::Define { database, word } => (
                &*DICT_DEFINE_EVENT,
                serde_json::json!({"database": database, "word": word}),
            ),
            Command::Match {
                database,
                strategy,
                word,
            } => (
                &*DICT_MATCH_EVENT,
                serde_json::json!({"database": database, "strategy": strategy, "word": word}),
            ),
            Command::ShowDatabases => (&*DICT_SHOW_EVENT, serde_json::json!({"what": "databases"})),
            Command::ShowStrategies => {
                (&*DICT_SHOW_EVENT, serde_json::json!({"what": "strategies"}))
            }
            Command::ShowInfo { database } => (
                &*DICT_SHOW_EVENT,
                serde_json::json!({"what": "info", "database": database}),
            ),
            Command::ShowServer => (&*DICT_SHOW_EVENT, serde_json::json!({"what": "server"})),
            _ => return None,
        };
        Some((event, data))
    }

    /// The success code a model reply to this command must carry. A 5xx is always acceptable.
    fn success_code(&self) -> Option<u16> {
        Some(match self {
            Command::Define { .. } => 150,
            Command::Match { .. } => 152,
            Command::ShowDatabases => 110,
            Command::ShowStrategies => 111,
            Command::ShowInfo { .. } => 112,
            Command::ShowServer => 114,
            _ => return None,
        })
    }

    /// NetGet's own reply to a command the model is not asked about, and whether to close.
    fn fixed_reply(&self) -> Option<(String, bool)> {
        let reply = match self {
            Command::Client | Command::OptionMime => "250 ok\r\n".to_string(),
            Command::Status => "210 status [netget]\r\n".to_string(),
            Command::Help => {
                let lines: Vec<String> = HELP_LINES.iter().map(|s| s.to_string()).collect();
                let mut out = wire::text_block("113 help text follows", &lines);
                out.push_str("250 ok\r\n");
                out
            }
            Command::Quit => return Some(("221 Closing Connection\r\n".to_string(), true)),
            Command::OptionOther => "503 Command parameter not implemented\r\n".to_string(),
            Command::NotImplemented => "502 Command not implemented\r\n".to_string(),
            Command::BadParameters => "501 Syntax error, illegal parameters\r\n".to_string(),
            Command::Unknown => "500 Syntax error, command not recognized\r\n".to_string(),
            _ => return None,
        };
        Some((reply, false))
    }
}

struct Session {
    peer_addr: SocketAddr,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    server_id: crate::state::ServerId,
    protocol: Arc<actions::DictProtocol>,
    connection_id: ConnectionId,
    first_timeout: Duration,
    idle_timeout: Duration,
}

/// What handling one command decided about the connection.
enum Next {
    Continue,
    Close,
}

impl Session {
    async fn run(self, socket: tokio::net::TcpStream) {
        let (mut reader, write_half) = tokio::io::split(socket);
        let write_half = Arc::new(Mutex::new(write_half));

        // Registered before the banner, so the operator can reach this connection while a
        // command is parked for them.
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

        self.session(&mut reader, &write_half).await;

        self.app_state
            .remove_peer_handle(self.server_id, self.connection_id.as_u32())
            .await;
        let _ = write_half.lock().await.shutdown().await;
        linger(&mut reader).await;
        self.app_state
            .update_connection_status(
                self.server_id,
                self.connection_id,
                crate::state::server::ConnectionStatus::Closed,
            )
            .await;
        let _ = self.status_tx.send("__UPDATE_UI__".to_string());
    }

    async fn write<W>(&self, write_half: &Arc<Mutex<W>>, data: &[u8]) -> std::io::Result<()>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        {
            let mut writer = write_half.lock().await;
            writer.write_all(data).await?;
            writer.flush().await?;
        }
        self.app_state
            .update_connection_stats(
                self.server_id,
                self.connection_id,
                None,
                Some(data.len() as u64),
                None,
                Some(1),
            )
            .await;
        Ok(())
    }

    async fn session<R, W>(&self, reader: &mut R, write_half: &Arc<Mutex<W>>)
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let log = Log::new(Some(&self.status_tx));
        let banner = format!(
            "220 netget DICT server <mime> <{}.{}.{}@netget>\r\n",
            self.connection_id.as_u32(),
            crate::utils::clock::process_id(),
            crate::utils::clock::SystemTime::now()
                .duration_since(crate::utils::clock::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        if self.write(write_half, banner.as_bytes()).await.is_err() {
            return;
        }

        let mut lines = LineReader::new(reader);
        let mut mime = false;
        let mut answered_one = false;

        loop {
            let timeout = if answered_one {
                self.idle_timeout
            } else {
                self.first_timeout
            };
            let (line, n) = match lines.next_line(timeout).await {
                LineRead::Line(line, n) => (line, n),
                LineRead::Eof => {
                    log.info(format!("DICT client {} disconnected", self.peer_addr));
                    return;
                }
                LineRead::TimedOut => {
                    log.info(format!(
                        "DICT client {} sent no command within {}s; closing",
                        self.peer_addr,
                        timeout.as_secs()
                    ));
                    return;
                }
                LineRead::TooLong => {
                    log.warn(format!(
                        "DICT command from {} exceeded {} bytes decision=fail_closed_line_too_long",
                        self.peer_addr, MAX_LINE_BYTES
                    ));
                    let _ = self
                        .write(write_half, b"500 Syntax error, command line too long\r\n")
                        .await;
                    return;
                }
                LineRead::Failed(e) => {
                    log.error(format!("DICT read error from {}: {}", self.peer_addr, e));
                    return;
                }
            };
            self.app_state
                .update_connection_stats(
                    self.server_id,
                    self.connection_id,
                    Some(n as u64),
                    None,
                    Some(1),
                    None,
                )
                .await;
            log.trace(format!("DICT command from {}: {:?}", self.peer_addr, line));

            if line.trim().is_empty() {
                continue;
            }
            answered_one = true;
            let command = parse_command(&line);

            if let Some((reply, close)) = command.fixed_reply() {
                if matches!(command, Command::OptionMime) {
                    mime = true;
                }
                if matches!(command, Command::Client) {
                    log.debug(format!(
                        "DICT client {} identified: {}",
                        self.peer_addr, line
                    ));
                }
                if self.write(write_half, reply.as_bytes()).await.is_err() || close {
                    return;
                }
                continue;
            }

            match self.answer_with_model(&command, write_half, mime).await {
                Next::Continue => {}
                Next::Close => return,
            }
        }
    }

    /// Raise the command's event, and write what the model decided — or `420` and close.
    async fn answer_with_model<W>(
        &self,
        command: &Command,
        write_half: &Arc<Mutex<W>>,
        mime: bool,
    ) -> Next
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let log = Log::new(Some(&self.status_tx));
        let (Some((event_type, data)), Some(success)) = (command.event(), command.success_code())
        else {
            return Next::Continue;
        };
        let event = Event::new(event_type, data);

        let result = match call_llm(
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
                let (category, reply) = match crate::utils::WireFailure::classify(&e) {
                    crate::utils::WireFailure::Overloaded => ("overloaded", OVERLOADED_REPLY),
                    crate::utils::WireFailure::Unavailable => ("unavailable", UNAVAILABLE_REPLY),
                };
                log.warn(format!(
                    "DICT {:?} from {} decision=fail_closed_llm_error category={}",
                    command, self.peer_addr, category
                ));
                log.debug(format!("DICT LLM call failed: {}", e));
                let _ = self.write(write_half, reply).await;
                return Next::Close;
            }
        };
        for message in &result.messages {
            log.info(message);
        }

        let mut replies: Vec<Vec<u8>> = Vec::new();
        let mut close = false;
        let mut stack = result.protocol_results;
        stack.reverse();
        while let Some(item) = stack.pop() {
            match item {
                ActionResult::Output(bytes) => replies.push(bytes),
                ActionResult::CloseConnection => close = true,
                ActionResult::Multiple(items) => stack.extend(items.into_iter().rev()),
                _ => {}
            }
        }

        let Some(reply) = replies.first() else {
            log.warn(format!(
                "DICT {:?} from {} decision=model_silent ({} failed action(s)); answering 420",
                command,
                self.peer_addr,
                result.failures.len()
            ));
            let _ = self.write(write_half, UNAVAILABLE_REPLY).await;
            return Next::Close;
        };

        let code = wire::leading_code(reply);
        let fits = matches!(code, Some(c) if c == success || (500..600).contains(&c));
        if !fits {
            log.warn(format!(
                "DICT {:?} from {} decision=fail_closed_mismatched_reply code={:?} (expected {} \
                 or 5xx); answering 420",
                command, self.peer_addr, code, success
            ));
            let _ = self.write(write_half, UNAVAILABLE_REPLY).await;
            return Next::Close;
        }
        if replies.len() > 1 {
            log.warn(format!(
                "DICT {:?}: the model produced {} replies to one command; sending the first",
                command,
                replies.len()
            ));
        }

        let decision = if code == Some(success) {
            "model_answer"
        } else {
            "model_reject"
        };
        log.info(format!(
            "DICT {:?} from {} decision={} code={}",
            command,
            self.peer_addr,
            decision,
            code.unwrap_or(0)
        ));

        let bytes = if mime {
            wire::apply_mime(reply)
        } else {
            reply.clone()
        };
        if self.write(write_half, &bytes).await.is_err() || close {
            return Next::Close;
        }
        Next::Continue
    }
}

/// How long, and how much, the server keeps reading after it has sent its last reply and
/// half-closed.
///
/// Closing a socket that still has unread input makes the kernel send RST instead of FIN, and
/// an RST can overtake the reply the peer has not read yet — so a peer refused for a 1025-byte
/// line, or told `420` with pipelined commands still in flight, would see "connection reset"
/// instead of the refusal. Reading and discarding what is already in flight turns the close into
/// a FIN. Bounded both ways so a peer that keeps sending cannot hold the task open.
const LINGER_TIME: Duration = Duration::from_secs(2);
const LINGER_BYTES: usize = 64 * 1024;

async fn linger<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) {
    let deadline = tokio::time::Instant::now() + LINGER_TIME;
    let mut sink = [0u8; 4096];
    let mut drained = 0usize;
    while drained < LINGER_BYTES {
        match tokio::time::timeout_at(deadline, reader.read(&mut sink)).await {
            Ok(Ok(n)) if n > 0 => drained += n,
            _ => break,
        }
    }
}

enum LineRead {
    /// A line without its terminator, and the wire bytes it consumed.
    Line(String, usize),
    Eof,
    TimedOut,
    /// The line exceeded [`MAX_LINE_BYTES`] including its CRLF.
    TooLong,
    Failed(std::io::Error),
}

/// Accumulates reads into lines, keeping anything after a newline for the next command —
/// RFC 2229 permits pipelining, and `dict(1)` does it.
struct LineReader<'a, R> {
    reader: &'a mut R,
    pending: Vec<u8>,
    chunk: Vec<u8>,
}

impl<'a, R: tokio::io::AsyncRead + Unpin> LineReader<'a, R> {
    fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            chunk: vec![0u8; 1024],
        }
    }

    /// `timeout` bounds the wait for *more bytes*, not the whole line.
    async fn next_line(&mut self, timeout: Duration) -> LineRead {
        loop {
            if let Some(idx) = self.pending.iter().position(|b| *b == b'\n') {
                let consumed = idx + 1;
                if consumed > MAX_LINE_BYTES {
                    return LineRead::TooLong;
                }
                let line: Vec<u8> = self.pending.drain(..consumed).collect();
                let text = String::from_utf8_lossy(&line[..idx]);
                return LineRead::Line(text.trim_end_matches('\r').to_string(), consumed);
            }
            // No newline in MAX_LINE_BYTES bytes: even if the next byte is one, the line is
            // longer than the RFC allows.
            if self.pending.len() >= MAX_LINE_BYTES {
                return LineRead::TooLong;
            }
            let read = match tokio::time::timeout(timeout, self.reader.read(&mut self.chunk)).await
            {
                Err(_) => return LineRead::TimedOut,
                Ok(read) => read,
            };
            match read {
                Ok(0) => {
                    if self.pending.is_empty() {
                        return LineRead::Eof;
                    }
                    let line = std::mem::take(&mut self.pending);
                    let consumed = line.len();
                    let text = String::from_utf8_lossy(&line);
                    return LineRead::Line(text.trim_end_matches('\r').to_string(), consumed);
                }
                Ok(n) => self.pending.extend_from_slice(&self.chunk[..n]),
                Err(e) => return LineRead::Failed(e),
            }
        }
    }
}
