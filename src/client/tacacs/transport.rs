use crate::protocol::ConnectContext;
use crate::server::tacacs::codec::{self, *};
use anyhow::{bail, ensure, Context, Result};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::net::TcpStream;
pub const DEFAULT_HANDLED_ARGUMENTS: [&str; 4] = ["service", "cmd", "cmd-arg", "priv-lvl"];
pub struct Settings {
    pub remote_addr: String,
    pub shared_secret: Vec<u8>,
    pub io_timeout: Duration,
}
pub enum Command {
    Authentication(Authentication),
    Authorization {
        request: Request,
        handled: Vec<String>,
    },
    Accounting {
        request: Request,
        kind: AccountKind,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizeAction {
    request: Request,
    #[serde(default)]
    handled_mandatory_arguments: Option<Vec<String>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountAction {
    request: Request,
    record_type: AccountKind,
}
pub fn parse(action: Value) -> Result<Command> {
    let mut action = codec::owned_json(action)?;
    ensure!(
        serde_json::to_vec(&action)?.len() <= 2 * MAX_BODY_BYTES,
        "action byte capacity"
    );
    let name = action
        .get("type")
        .and_then(Value::as_str)
        .context("action type")?
        .to_string();
    action
        .as_object_mut()
        .context("action object")?
        .remove("type");
    match name.as_str() {
        "authenticate_tacacs" => {
            let a: Authentication = serde_json::from_value(action)?;
            authentication_body(&a)?;
            Ok(Command::Authentication(a))
        }
        "authorize_tacacs" => {
            let a: AuthorizeAction = serde_json::from_value(action)?;
            request_body(&a.request, None)?;
            let handled = a.handled_mandatory_arguments.unwrap_or_else(|| {
                DEFAULT_HANDLED_ARGUMENTS
                    .iter()
                    .map(|s| (*s).into())
                    .collect()
            });
            ensure!(
                handled.len() <= MAX_ARGUMENTS,
                "handled argument capacity32"
            );
            for name in &handled {
                text(name, MAX_TEXT_BYTES)?;
                ensure!(
                    !name.is_empty() && !name.contains(['=', '*']),
                    "handled argument name"
                );
            }
            Ok(Command::Authorization {
                request: a.request,
                handled,
            })
        }
        "account_tacacs" => {
            let a: AccountAction = serde_json::from_value(action)?;
            request_body(&a.request, Some(a.record_type))?;
            Ok(Command::Accounting {
                request: a.request,
                kind: a.record_type,
            })
        }
        _ => bail!("Unknown TACACS client action"),
    }
}
pub struct Response {
    pub event: &'static str,
    pub data: Value,
}
struct Session {
    socket: TcpStream,
    header: Header,
    settings: Arc<Settings>,
    ctx: ConnectContext,
}
impl Session {
    async fn send(&mut self, body: &[u8]) -> Result<()> {
        let n = write_packet(
            &mut self.socket,
            self.header,
            body,
            &self.settings.shared_secret,
        )
        .await?;
        stats(&self.ctx, n as u64, false).await;
        Ok(())
    }
    async fn read(&mut self) -> Result<Vec<u8>> {
        let (header, body) = read_packet(
            &mut self.socket,
            &self.settings.shared_secret,
            self.settings.io_timeout,
        )
        .await?;
        stats(&self.ctx, (12 + header.length) as u64, true).await;
        header.validate()?;
        ensure!(
            header.session_id == self.header.session_id
                && header.kind == self.header.kind
                && header.version == self.header.version
                && header.sequence
                    == self
                        .header
                        .sequence
                        .checked_add(1)
                        .context("sequence must never wrap")?,
            "response session/type/version/sequence mismatch"
        );
        self.header = header;
        Ok(body)
    }
    fn next(&mut self) -> Result<()> {
        self.header.sequence = self
            .header
            .sequence
            .checked_add(1)
            .context("sequence must never wrap")?;
        self.header.flags = 0;
        Ok(())
    }
}
pub async fn exchange(
    settings: Arc<Settings>,
    command: Command,
    ctx: ConnectContext,
) -> Result<Response> {
    let (version, kind, body) = match &command {
        Command::Authentication(a) => {
            let (v, b) = authentication_body(a)?;
            (v, 1, b)
        }
        Command::Authorization { request, .. } => (0xc0, 2, request_body(request, None)?),
        Command::Accounting { request, kind } => (0xc0, 3, request_body(request, Some(*kind))?),
    };
    let mut random = [0; 4];
    rand::rngs::OsRng
        .try_fill_bytes(&mut random)
        .context("session RNG")?;
    let header = Header {
        version,
        kind,
        sequence: 1,
        flags: 0,
        session_id: u32::from_be_bytes(random),
        length: 0,
    };
    let socket = tokio::time::timeout(
        settings.io_timeout,
        TcpStream::connect(&settings.remote_addr),
    )
    .await
    .context("connect deadline")??;
    let connected_addr = socket.peer_addr()?;
    let local_addr = socket.local_addr()?;
    ctx.state
        .with_client_mut(ctx.client_id, |client| {
            let now = crate::utils::clock::Instant::now();
            let connection = client.connection.get_or_insert_with(|| {
                crate::state::client::ClientConnectionState {
                    id: ctx.client_id,
                    remote_addr: settings.remote_addr.clone(),
                    connected_addr: None,
                    local_addr: None,
                    bytes_sent: 0,
                    bytes_received: 0,
                    packets_sent: 0,
                    packets_received: 0,
                    last_activity: now,
                    status: crate::state::ClientStatus::Connected,
                    status_changed_at: now,
                    protocol_info: crate::state::server::ProtocolConnectionInfo::empty(),
                }
            });
            connection.connected_addr = Some(connected_addr);
            connection.local_addr = Some(local_addr);
        })
        .await;
    let mut session = Session {
        socket,
        header,
        settings,
        ctx,
    };
    session.send(&body).await?;
    let mut body = session.read().await?;
    let response = match command {
        Command::Authentication(a) => {
            let mut terminal = None;
            for round in 0..MAX_AUTH_ROUNDS {
                let reply = parse_auth_reply(&body)?;
                match reply.status {
                    AuthStatus::GetUser | AuthStatus::GetPass
                        if a.method == AuthType::Ascii && round + 1 < MAX_AUTH_ROUNDS =>
                    {
                        session.next()?;
                        session
                            .send(&continue_body(
                                if reply.status == AuthStatus::GetUser {
                                    &a.username
                                } else {
                                    &a.password
                                },
                                false,
                            )?)
                            .await?;
                        body = session.read().await?;
                    }
                    AuthStatus::GetData | AuthStatus::GetUser | AuthStatus::GetPass => {
                        session.next()?;
                        let _ = session
                            .send(&continue_body("Unsupported prompt or round limit", true)?)
                            .await;
                        bail!("Unsupported prompt or authentication round capacity");
                    }
                    _ => {
                        terminal = Some(reply);
                        break;
                    }
                }
            }
            let mut reply = terminal.context("authentication round capacity")?;
            let server_status = reply.status;
            let allowed = reply.status == AuthStatus::Pass;
            if matches!(reply.status, AuthStatus::Restart | AuthStatus::Follow) {
                reply.status = AuthStatus::Fail;
            }
            Response {
                event: "tacacs_authentication_result",
                data: json!({"request":{"username":a.username,"method":a.method,"privilege_level":a.privilege_level,"port":a.port,"remote_address":a.remote_address},"reply":reply,"server_status":server_status,"authenticated":allowed,"session_id":header.session_id}),
            }
        }
        Command::Authorization { request, handled } => {
            let reply = parse_author_reply(&body)?;
            let effective = match reply.status {
                AuthorStatus::PassAdd => request
                    .arguments
                    .iter()
                    .chain(&reply.arguments)
                    .cloned()
                    .collect::<Vec<_>>(),
                AuthorStatus::PassReplace => reply.arguments.clone(),
                _ => vec![],
            };
            let unhandled = effective
                .iter()
                .filter(|a| a.mandatory && !handled.contains(&a.name))
                .map(|a| a.name.clone())
                .collect::<Vec<_>>();
            let valid_privilege = effective
                .iter()
                .filter(|a| a.name == "priv-lvl")
                .all(|a| a.value.parse::<u8>().is_ok_and(|v| v <= 15));
            let authorized = matches!(
                reply.status,
                AuthorStatus::PassAdd | AuthorStatus::PassReplace
            ) && unhandled.is_empty()
                && valid_privilege;
            let ignored = effective
                .iter()
                .filter(|a| !a.mandatory && !handled.contains(&a.name))
                .cloned()
                .collect::<Vec<_>>();
            let effective = if authorized {
                effective
                    .into_iter()
                    .filter(|a| handled.contains(&a.name))
                    .collect::<Vec<_>>()
            } else {
                vec![]
            };
            Response {
                event: "tacacs_authorization_result",
                data: json!({"request":request,"reply":reply,"authorized":authorized,"unhandled_mandatory_arguments":unhandled,"ignored_optional_arguments":ignored,"effective_arguments":effective,"session_id":header.session_id,"device_policy_applied":false}),
            }
        }
        Command::Accounting { request, kind } => {
            let reply = parse_account_reply(&body)?;
            let success = reply.status == AccountStatus::Success;
            Response {
                event: "tacacs_accounting_result",
                data: json!({"request":request,"record_type":kind,"reply":reply,"recorded_by_peer":success,"durable_storage_confirmed":false,"session_id":header.session_id}),
            }
        }
    };
    Ok(response)
}

async fn stats(ctx: &ConnectContext, bytes: u64, received: bool) {
    ctx.state
        .with_client_mut(ctx.client_id, |client| {
            if let Some(client) = client.connection.as_mut() {
                if received {
                    client.bytes_received += bytes;
                    client.packets_received += 1;
                } else {
                    client.bytes_sent += bytes;
                    client.packets_sent += 1;
                }
                client.last_activity = crate::utils::clock::Instant::now();
            }
        })
        .await;
}
