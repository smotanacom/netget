//! Sequential, bounded NNTP transactions over TCP or verified implicit TLS.
pub mod actions;
use crate::server::p2p_support::{Framer, ScannerSession, Stream};
pub use actions::NntpClientProtocol;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
#[derive(Default)]
pub struct Session {
    frame: Framer,
    welcome: String,
    tls: bool,
}
impl Session {
    async fn response(&mut self, s: &mut Stream, command: &str) -> Result<Value> {
        let raw = self.frame.delimited(s, b'\n', 64 * 1024).await?;
        ensure!(raw.ends_with(b"\r\n"), "NNTP response must end in CRLF");
        let line = String::from_utf8(raw)?;
        ensure!(
            line.len() >= 5 && line.as_bytes()[3] == b' ',
            "invalid NNTP status line"
        );
        let code: u16 = line[..3].parse()?;
        ensure!((100..600).contains(&code), "invalid status code");
        let mut response = line.trim_end_matches("\r\n").to_string();
        if matches!(
            code,
            100 | 101 | 215 | 220 | 221 | 222 | 224 | 225 | 230 | 231
        ) {
            loop {
                let raw = self.frame.delimited(s, b'\n', 64 * 1024).await?;
                ensure!(raw.ends_with(b"\r\n"), "multiline NNTP framing");
                let text = std::str::from_utf8(&raw)?.trim_end_matches("\r\n");
                if text == "." {
                    break;
                }
                let text = if text.starts_with("..") {
                    &text[1..]
                } else {
                    text
                };
                ensure!(
                    response.len() + text.len() + 1 <= 8 * 1024 * 1024,
                    "NNTP multiline reply too large"
                );
                response.push('\n');
                response.push_str(text);
            }
        }
        let safe_command = if command.to_ascii_uppercase().starts_with("AUTHINFO PASS ") {
            "AUTHINFO PASS [redacted]"
        } else {
            command
        };
        Ok(json!({"command":safe_command,"code":code,"status_code":code,"response":response}))
    }
    async fn command(&mut self, s: &mut Stream, command: &str) -> Result<Value> {
        ensure!(
            command.len() <= 510 && !command.contains(['\r', '\n', '\0']),
            "invalid NNTP command"
        );
        s.write_all(format!("{command}\r\n").as_bytes()).await?;
        self.response(s, command).await
    }
}
#[async_trait]
impl ScannerSession for Session {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        let v = self.response(s, "GREETING").await?;
        ensure!(
            v["code"] == 200 || v["code"] == 201,
            "NNTP greeting refused"
        );
        self.welcome = v["response"].as_str().context("welcome")?.to_string();
        Ok(())
    }
    fn outcome(
        &self,
        a: &Value,
        response: &Value,
    ) -> crate::state::client_handles::ClientSendOutcome {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};
        if let Ok(ClientActionResult::Custom { name, data }) =
            NntpClientProtocol::new().execute_action(a.clone())
        {
            if name == "nntp_command" {
                return crate::state::client_handles::ClientSendOutcome::Sent {
                    bytes_sent: data["command"].as_str().map_or(0, |s| s.len() + 2),
                };
            }
        }
        crate::state::client_handles::ClientSendOutcome::Executed {
            detail: response.to_string(),
        }
    }
    async fn close(&mut self, s: &mut Stream) -> Result<()> {
        let result = self.command(s, "QUIT").await?;
        ensure!(result["code"] == 205, "NNTP QUIT refused");
        Ok(())
    }
    fn connected(&self) -> Value {
        json!({"welcome_message":self.welcome})
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};
        match NntpClientProtocol::new().execute_action(a.clone())? {
            ClientActionResult::Custom { name, data } if name == "nntp_command" => {
                self.command(s, data["command"].as_str().context("command")?)
                    .await
            }
            ClientActionResult::Custom { name, data } if name == "nntp_authenticate" => {
                ensure!(self.tls, "NNTP credentials require implicit TLS");
                let user = crate::server::nntp::extensions::field(
                    data["username"].as_str().context("username")?,
                )?;
                let password = crate::server::nntp::extensions::field(
                    data["password"].as_str().context("password")?,
                )?;
                let result = self.command(s, &format!("AUTHINFO USER {user}")).await?;
                if result["code"] != 381 {
                    return Ok(result);
                }
                self.command(s, &format!("AUTHINFO PASS {password}")).await
            }
            ClientActionResult::Custom { name, data }
                if matches!(name.as_str(), "nntp_post" | "nntp_ihave" | "nntp_takethis") =>
            {
                let article = crate::server::nntp::extensions::article(&data)?;
                let (command, continue_code) = if name == "nntp_post" {
                    ("POST".to_string(), 340)
                } else {
                    let id = crate::server::nntp::extensions::message_id(
                        data["message_id"].as_str().context("message_id")?,
                    )?;
                    (
                        format!(
                            "{} {id}",
                            if name == "nntp_ihave" {
                                "IHAVE"
                            } else {
                                "TAKETHIS"
                            }
                        ),
                        335,
                    )
                };
                if name == "nntp_takethis" {
                    s.write_all(format!("{command}\r\n").as_bytes()).await?;
                } else {
                    let response = self.command(s, &command).await?;
                    if response["code"] != continue_code {
                        return Ok(response);
                    }
                }
                s.write_all(&article).await?;
                self.response(s, &command).await
            }
            _ => anyhow::bail!("unsupported NNTP transaction"),
        }
    }
}
pub async fn connect(ctx: crate::protocol::ConnectContext) -> Result<std::net::SocketAddr> {
    let tls = crate::server::p2p_support::use_tls(ctx.startup_params.as_ref())?;
    crate::server::p2p_support::connect(
        ctx,
        std::sync::Arc::new(NntpClientProtocol::new()),
        Session {
            tls,
            ..Default::default()
        },
        [&actions::NNTP_CLIENT_CONNECTED_EVENT, &actions::NNTP_CLIENT_RESPONSE_RECEIVED_EVENT],
    )
    .await
}
