//! NNTP posting, AUTHINFO USER/PASS and article-feed transactions.
use crate::llm::actions::{client_trait::ClientActionResult, ActionDefinition};
use crate::server::p2p_support::{action, parameter};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
pub const MAX_ARTICLE: usize = 8 * 1024 * 1024;
pub fn field(text: &str) -> Result<&str> {
    ensure!(
        !text.is_empty() && text.len() <= 4096 && !text.contains(['\r', '\n', '\0']),
        "invalid NNTP field"
    );
    Ok(text)
}
pub fn message_id(text: &str) -> Result<&str> {
    field(text)?;
    ensure!(
        text.starts_with('<')
            && text.ends_with('>')
            && !text.bytes().any(|b| b <= 32)
            && text.contains('@'),
        "invalid message ID"
    );
    Ok(text)
}
pub fn dot_block(text: &str) -> Result<Vec<u8>> {
    ensure!(text.len() <= MAX_ARTICLE, "article too large");
    let normalized = text.replace("\r\n", "\n");
    ensure!(
        !normalized.contains(['\r', '\0']),
        "invalid article line ending"
    );
    let mut out = Vec::new();
    for line in normalized.split_terminator('\n') {
        ensure!(line.len() <= 64 * 1024 - 2, "article line too long");
        if line.starts_with('.') {
            out.push(b'.');
        }
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    ensure!(out.len() <= MAX_ARTICLE, "encoded article too large");
    out.extend_from_slice(b".\r\n");
    Ok(out)
}
pub fn article(v: &Value) -> Result<Vec<u8>> {
    let headers = v["headers"]
        .as_object()
        .context("headers object required")?;
    let mut text = String::new();
    ensure!(!headers.is_empty() && headers.len() <= 128, "header count");
    for (key, val) in headers {
        ensure!(
            !key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid header name"
        );
        text.push_str(key);
        text.push_str(": ");
        text.push_str(field(val.as_str().context("header value")?)?);
        text.push('\n');
    }
    text.push('\n');
    text.push_str(v["body"].as_str().context("body required")?);
    dot_block(&text)
}
pub fn client_actions() -> Vec<ActionDefinition> {
    let mut list = vec![
        action(
            "nntp_capabilities",
            "Discover server capabilities",
            vec![],
            json!({"type":"nntp_capabilities"}),
        ),
        action(
            "nntp_mode_stream",
            "Enter streaming-feed mode",
            vec![],
            json!({"type":"nntp_mode_stream"}),
        ),
        action(
            "nntp_check",
            "Offer an article message ID",
            vec![parameter(
                "message_id",
                "string",
                "Article message ID in angle brackets, such as <article@example.org>",
                true,
            )],
            json!({"type":"nntp_check","message_id":"<test@localhost>"}),
        ),
        action(
            "nntp_authenticate",
            "Authenticate over verified TLS",
            vec![
                parameter("username", "string", "Account username", true),
                parameter("password", "string", "Account password", true),
            ],
            json!({"type":"nntp_authenticate","username":"user","password":"secret"}),
        ),
    ];
    for name in ["nntp_ihave", "nntp_takethis"] {
        list.push(action(name,"Feed an article to another news server",vec![parameter("message_id","string","RFC message ID matching the article",true),parameter("headers","object","Article header fields",true),parameter("body","string","Article body text; line normalization and dot stuffing are automatic",true)],json!({"type":name,"message_id":"<test@localhost>","headers":{"Message-ID":"<test@localhost>","From":"test@localhost","Newsgroups":"misc.test","Subject":"Test"},"body":"Hello"})));
    }
    list
}
pub fn client_action(v: &Value) -> Result<Option<ClientActionResult>> {
    let name = v["type"].as_str().context("type")?;
    let command = match name {
        "nntp_capabilities" => Some("CAPABILITIES".to_string()),
        "nntp_mode_stream" => Some("MODE STREAM".to_string()),
        "nntp_check" => Some(format!(
            "CHECK {}",
            message_id(v["message_id"].as_str().context("message_id")?)?
        )),
        "nntp_authenticate" => {
            field(v["username"].as_str().context("username")?)?;
            field(v["password"].as_str().context("password")?)?;
            return Ok(Some(ClientActionResult::Custom {
                name: name.into(),
                data: v.clone(),
            }));
        }
        "nntp_ihave" | "nntp_takethis" => {
            let id = message_id(v["message_id"].as_str().context("message_id")?)?;
            article(v)?;
            ensure!(
                v["headers"]
                    .as_object()
                    .context("headers")?
                    .iter()
                    .any(|(k, val)| k.eq_ignore_ascii_case("message-id") && val == id),
                "article message ID mismatch"
            );
            return Ok(Some(ClientActionResult::Custom {
                name: name.into(),
                data: v.clone(),
            }));
        }
        _ => return Ok(None),
    };
    Ok(command.map(|command| ClientActionResult::Custom {
        name: "nntp_command".into(),
        data: json!({"command":command}),
    }))
}
pub fn server_actions() -> Vec<ActionDefinition> {
    [
        "nntp_auth_result",
        "nntp_check_result",
        "nntp_article_result",
    ]
    .into_iter()
    .map(|name| {
        action(
            name,
            "Accept or refuse this authentication, offer or received article",
            vec![parameter(
                "accepted",
                "boolean",
                "Accept this request; omitted or false refuses",
                true,
            )],
            json!({"type":name,"accepted":false}),
        )
    })
    .collect()
}
#[derive(Default)]
pub struct State {
    pub authenticated: bool,
    pub secure: bool,
    pub username: Option<String>,
    pub streaming: bool,
    pub require_auth: bool,
    pub posting_allowed: bool,
}
pub struct ContextRefs<'a> {
    pub llm: &'a crate::llm::OllamaClient,
    pub state: &'a Arc<crate::state::AppState>,
    pub server: crate::state::ServerId,
    pub peer: crate::server::connection::ConnectionId,
    pub protocol: &'a super::NntpProtocol,
}
async fn decision(ctx: &ContextRefs<'_>, event: Value, kind: &str) -> bool {
    let event = crate::protocol::Event::new(&super::actions::NNTP_COMMAND_RECEIVED_EVENT, event);
    let Ok(result) = crate::llm::action_helper::call_llm(
        ctx.llm,
        ctx.state,
        ctx.server,
        Some(ctx.peer),
        &event,
        ctx.protocol,
    )
    .await
    else {
        return false;
    };
    result
        .protocol_results
        .iter()
        .find_map(|v| {
            if let crate::llm::ActionResult::Custom { name, data } = v {
                (name == kind).then(|| data["accepted"].as_bool().unwrap_or(false))
            } else {
                None
            }
        })
        .unwrap_or(false)
}
async fn write<W: AsyncWrite + Unpin>(
    writer: &Arc<tokio::sync::Mutex<W>>,
    text: &str,
) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        writer.lock().await.write_all(text.as_bytes()).await
    })
    .await??;
    Ok(())
}
/// Return true when an extension consumed the command (and its article, when present).
pub async fn handle<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut BufReader<R>,
    writer: &Arc<tokio::sync::Mutex<W>>,
    line: &str,
    state: &mut State,
    ctx: &ContextRefs<'_>,
) -> Result<bool> {
    let words: Vec<_> = line.split_whitespace().collect();
    let verb = words
        .first()
        .map(|v| v.to_ascii_uppercase())
        .unwrap_or_default();
    if verb == "AUTHINFO" {
        let mut auth = line.splitn(3, ' ');
        auth.next();
        let mode = auth.next().unwrap_or("").to_ascii_uppercase();
        let value = auth.next().unwrap_or("");
        match mode.as_str() {
            "USER" if !value.is_empty() => {
                if !state.secure {
                    write(writer, "483 TLS required\r\n").await?;
                } else {
                    state.username = Some(field(value)?.into());
                    write(writer, "381 Password required\r\n").await?;
                }
            }
            "PASS" if !value.is_empty() => {
                if !state.secure {
                    write(writer, "483 TLS required\r\n").await?;
                } else if let Some(user) = state.username.take() {
                    let accepted=decision(ctx,json!({"command":"AUTHINFO PASS","operation":"authenticate","username":user,"password":field(value)?,"answer_with":"nntp_auth_result"}),"nntp_auth_result").await;
                    state.authenticated = accepted;
                    write(
                        writer,
                        if accepted {
                            "281 Authentication accepted\r\n"
                        } else {
                            "481 Authentication rejected\r\n"
                        },
                    )
                    .await?;
                } else {
                    write(writer, "482 Send AUTHINFO USER first\r\n").await?;
                }
            }
            _ => write(writer, "501 Invalid AUTHINFO syntax\r\n").await?,
        }
        return Ok(true);
    }
    if state.require_auth
        && !state.authenticated
        && !matches!(verb.as_str(), "CAPABILITIES" | "QUIT")
    {
        write(writer, "480 Authentication required\r\n").await?;
        return Ok(true);
    }
    if verb == "MODE" && words.len() == 2 && words[1].eq_ignore_ascii_case("STREAM") {
        state.streaming = true;
        write(writer, "203 Streaming permitted\r\n").await?;
        return Ok(true);
    }
    if verb == "CHECK" {
        if !state.streaming {
            write(writer, "500 Enter MODE STREAM first\r\n").await?;
            return Ok(true);
        }
        if words.len() != 2 || message_id(words[1]).is_err() {
            write(writer, "501 Invalid message ID\r\n").await?;
            return Ok(true);
        }
        let accepted=decision(ctx,json!({"command":line,"operation":"check","message_id":words[1],"answer_with":"nntp_check_result"}),"nntp_check_result").await;
        write(
            writer,
            &format!("{} {}\r\n", if accepted { 238 } else { 438 }, words[1]),
        )
        .await?;
        return Ok(true);
    }
    if verb == "MODE"
        && words
            .get(1)
            .is_some_and(|v| v.eq_ignore_ascii_case("READER"))
    {
        state.streaming = false;
    }
    if !matches!(verb.as_str(), "POST" | "IHAVE" | "TAKETHIS") {
        return Ok(false);
    }
    if verb == "POST" && !state.posting_allowed {
        write(writer, "440 Posting prohibited\r\n").await?;
        return Ok(true);
    }
    if (verb == "POST" && words.len() != 1)
        || (verb != "POST" && (words.len() != 2 || message_id(words[1]).is_err()))
    {
        write(writer, "501 Invalid article command\r\n").await?;
        return Ok(true);
    }
    let id = if verb == "POST" {
        None
    } else {
        Some(message_id(words[1])?)
    };
    if verb == "TAKETHIS" {
        if !state.streaming {
            write(writer, "500 Enter MODE STREAM first\r\n").await?;
            return Ok(true);
        }
    } else {
        write(
            writer,
            if verb == "POST" {
                "340 Send article\r\n"
            } else {
                "335 Send article\r\n"
            },
        )
        .await?;
    }
    let data = crate::client::response_reader::read_dot_response(reader, String::new()).await?;
    let article = data.strip_prefix('\n').unwrap_or(&data);
    let valid_id = id.is_none_or(|id| {
        article.lines().take_while(|l| !l.is_empty()).any(|l| {
            l.split_once(':').is_some_and(|(key, value)| {
                key.eq_ignore_ascii_case("message-id") && value.trim() == id
            })
        })
    });
    let accepted=valid_id&&decision(ctx,json!({"command":line,"operation":verb.to_ascii_lowercase(),"article":article,"message_id":id,"answer_with":"nntp_article_result"}),"nntp_article_result").await;
    let code = match (verb.as_str(), accepted) {
        ("POST", true) => 240,
        ("POST", false) => 441,
        ("IHAVE", true) => 235,
        ("IHAVE", false) => 437,
        ("TAKETHIS", true) => 239,
        _ => 439,
    };
    write(
        writer,
        &format!(
            "{code} {}\r\n",
            id.unwrap_or(if accepted {
                "Article accepted"
            } else {
                "Article rejected"
            })
        ),
    )
    .await?;
    Ok(true)
}
