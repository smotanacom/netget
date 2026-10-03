//! The DoQ peer is the official AdGuard dnsproxy binary (quic-go + miekg/dns).
//! It resolves a temporary hosts file; neither endpoint contacts external DNS.
use netget::{
    cli::management::ClientForm,
    llm::OllamaClient,
    state::{client_handles::ClientSendOutcome, AccessLogOwner, AppState},
};
use serde_json::json;
use std::{process::Stdio, time::Duration};

#[tokio::test]
async fn real_adguard_dnsproxy_answers_client_queries_and_followups() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let issued = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = dir.path().join("cert.pem");
    let key = dir.path().join("key.pem");
    let hosts = dir.path().join("hosts");
    std::fs::write(&cert, issued.cert.pem()).unwrap();
    std::fs::write(&key, issued.signing_key.serialize_pem()).unwrap();
    std::fs::write(
        &hosts,
        "192.0.2.61 independent.example\n2001:db8::61 independent.example\n",
    )
    .unwrap();
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = std::env::var("NETGET_DNSPROXY_BIN").unwrap_or_else(|_| "dnsproxy".into());
    let mut peer=tokio::process::Command::new(binary)
        .args(["--listen=127.0.0.1","--port=0","--upstream=127.0.0.1:9","--hosts-file-enabled"])
        .arg(format!("--quic-port={port}")).arg(format!("--tls-crt={}",cert.display()))
        .arg(format!("--tls-key={}",key.display())).arg(format!("--hosts-files={}",hosts.display()))
        .stdout(Stdio::null()).stderr(Stdio::inherit()).kill_on_drop(true).spawn()
        .expect("Install official AdguardTeam/dnsproxy release or set NETGET_DNSPROXY_BIN; this test never skips");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                peer.try_wait().unwrap().is_none(),
                "dnsproxy exited before binding"
            );
            if std::net::UdpSocket::bind(("127.0.0.1", port)).is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let llm = OllamaClient::new("http://127.0.0.1:1");
    state.set_llm_client(llm.clone()).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let client=ClientForm {
        protocol:"doq".into(),remote_addr:Some(format!("127.0.0.1:{port}")),instruction:Some("Query the independent resolver".into()),
        startup_params:Some(json!({"server_name":"localhost","ca_cert_path":cert})),
        event_handlers:Some(vec![
            json!({"event_pattern":"doq_response_received","handler":{"type":"script","language":"python","code":"import json,sys\nevent=json.load(sys.stdin)['event']\ndef respond(actions):\n    print(json.dumps({'actions':actions}))\nif event['query_type']=='A':\n    respond([{'type':'send_dns_query','domain':'independent.example','query_type':'AAAA'}])\nelse:\n    respond([])"}}),
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),..Default::default()
    }.create(&state,llm,tx).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !state.has_client_handle(client).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let result = state
        .send_to_client(
            client,
            json!({"type":"send_dns_query","domain":"independent.example","query_type":"A"}),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(matches!(result, ClientSendOutcome::Executed { .. }));
    let owner = AccessLogOwner::Client(client.as_u32());
    super::support::wait_log(&state, owner, "192.0.2.61").await;
    super::support::wait_log(&state, owner, "2001:db8::61").await;
    state.remove_client(client).await;
    peer.kill().await.unwrap();
    peer.wait().await.unwrap();
}
