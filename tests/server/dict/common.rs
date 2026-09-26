//! Helpers shared by the DICT suites: an in-process `AppState` whose model endpoint is a dead
//! port, a server started through the same `ServerForm` path the dashboard uses, and a
//! line-oriented raw DICT peer.

#![allow(dead_code)]

use std::time::Duration;

use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::ServerId;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// Not listening: any call that reaches the model fails with a transport error.
pub const DEAD_LLM: &str = "http://127.0.0.1:1";

pub async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, DEAD_LLM.to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(DEAD_LLM.to_string()))
        .await;
    state
}

pub async fn wait_for_port(state: &AppState, id: ServerId) -> u16 {
    for _ in 0..300 {
        if let Some(s) = state.get_server(id).await {
            if let Some(addr) = s.local_addr {
                return addr.port();
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("DICT server #{} never bound a port", id.as_u32());
}

/// Start a DICT server with the given handlers and startup parameters. The instruction is
/// empty, so nothing is answered by a default instruction behind the handlers' backs.
pub async fn start(
    state: &AppState,
    handlers: Vec<serde_json::Value>,
    startup_params: Option<serde_json::Value>,
) -> (ServerId, u16, mpsc::UnboundedReceiver<String>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let server_id = ServerForm {
        protocol: "dict".to_string(),
        port: Some(0),
        instruction: Some(String::new()),
        startup_params,
        event_handlers: if handlers.is_empty() {
            None
        } else {
            Some(handlers)
        },
        ..Default::default()
    }
    .create(state, tx)
    .await
    .expect("create dict server");
    let port = wait_for_port(state, server_id).await;
    (server_id, port, rx)
}

/// A deterministic dictionary, as a Python script handler (no model involved).
///
/// `fantasy` holds `glimmerwyrm` with a definition whose lines exercise dot-stuffing: one line
/// that begins with a dot and one that *is* a dot, either of which ends the text block early
/// if NetGet does not stuff it. `nosuchword` has no definitions.
pub const DICTIONARY_SCRIPT: &str = r#"import json, sys
i = json.load(sys.stdin)
t = i['event_type_id']
e = i['event']
if t == 'dict_define':
    w = e['word']
    if w == 'nosuchword':
        a = [{'type': 'send_dict_definitions', 'word': w, 'definitions': []}]
    else:
        a = [{'type': 'send_dict_definitions', 'word': w, 'definitions': [
            {'database': 'fantasy', 'database_description': 'Fantasy Lexicon',
             'text': w + '\n  n. a small dragon that hoards moonlight\n.a line that begins with a dot\n.\nlast line'},
            {'database': 'tech', 'database_description': 'Technical Terms',
             'text': w + '\n  n. a caching proxy named after a dragon'}]}]
elif t == 'dict_match':
    if e['strategy'] == 'prefix':
        a = [{'type': 'send_dict_matches', 'matches': [
            {'database': 'fantasy', 'word': e['word'] + 'wyrm'},
            {'database': 'fantasy', 'word': e['word'] + 'fox'}]}]
    else:
        a = [{'type': 'send_dict_matches', 'matches': []}]
elif t == 'dict_show':
    what = e['what']
    if what == 'databases':
        a = [{'type': 'send_dict_databases', 'databases': [
            {'name': 'fantasy', 'description': 'Fantasy Lexicon'},
            {'name': 'tech', 'description': 'Technical Terms'}]}]
    elif what == 'strategies':
        a = [{'type': 'send_dict_strategies', 'strategies': [
            {'name': 'exact', 'description': 'Match headwords exactly'},
            {'name': 'prefix', 'description': 'Match prefixes'}]}]
    elif what == 'info':
        if e.get('database') == 'fantasy':
            a = [{'type': 'send_dict_text', 'code': 112, 'text': 'The Fantasy Lexicon\nInvented words, invented meanings.'}]
        else:
            a = [{'type': 'send_dict_error', 'code': 550}]
    else:
        a = [{'type': 'send_dict_text', 'code': 114, 'text': 'NetGet test dictionary\nEvery answer comes from a script.'}]
else:
    a = []
print(json.dumps({'actions': a}))
"#;

pub fn dictionary_handler() -> serde_json::Value {
    serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "script", "language": "python", "code": DICTIONARY_SCRIPT}
    })
}

/// A raw DICT peer that reads CRLF lines.
pub struct Peer {
    pub reader: BufReader<TcpStream>,
}

impl Peer {
    pub async fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        Self {
            reader: BufReader::new(stream),
        }
    }

    /// Read one line, terminator included. Empty string on EOF.
    pub async fn line(&mut self, secs: u64) -> String {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(secs), self.reader.read_line(&mut line))
            .await
            .unwrap_or_else(|_| panic!("no line from the DICT server within {secs}s"))
            .expect("read line");
        line
    }

    /// Read lines up to and including one that starts with `code ` (a final status).
    pub async fn until_status(&mut self, codes: &[&str], secs: u64) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let line = self.line(secs).await;
            assert!(!line.is_empty(), "EOF before a final status; got {lines:?}");
            let done = codes.iter().any(|c| line.starts_with(&format!("{c} ")));
            lines.push(line);
            if done {
                return lines;
            }
        }
    }

    pub async fn send(&mut self, command: &str) {
        let stream = self.reader.get_mut();
        stream
            .write_all(format!("{command}\r\n").as_bytes())
            .await
            .expect("write");
        stream.flush().await.expect("flush");
    }
}

/// Drain everything the server logged so far.
pub fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(line) = rx.try_recv() {
        out.push(line);
    }
    out
}

/// Wait until a log line containing `needle` arrives; returns everything seen.
pub async fn wait_for_log(
    rx: &mut mpsc::UnboundedReceiver<String>,
    needle: &str,
    secs: u64,
) -> Vec<String> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(line)) => {
                let hit = line.contains(needle);
                seen.push(line);
                if hit {
                    return seen;
                }
            }
            _ => panic!("no log line containing {needle:?} within {secs}s; saw {seen:#?}"),
        }
    }
}
