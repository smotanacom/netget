//! NetGet's Consul client against the official consul agent (`agent -dev`, 1.20.2), failing
//! rather than skipping when absent. The client's handlers write a key, read it back and
//! register a service; the consul CLI — the agent's own view — confirms both.
use super::session_test::{client, send, wait_for};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

fn consul_bin() -> PathBuf {
    std::env::var_os("NETGET_CONSUL_BIN").map(PathBuf::from).unwrap_or_else(|| {
        panic!("NETGET_CONSUL_BIN is required: run tests/server/consul/install_peers.py <root> and export what it prints")
    })
}

#[tokio::test]
async fn netget_against_consul_agent() {
    let bin = consul_bin();
    let agent = RealServer::builder(
        bin.to_str().unwrap(),
        InstallHint {
            brew: "consul",
            apt: "consul (or tests/server/consul/install_peers.py)",
        },
    )
    .args([
        "agent",
        "-dev",
        "-bind",
        "127.0.0.1",
        "-client",
        "127.0.0.1",
        "-http-port",
        "{port}",
        "-dns-port",
        "-1",
        "-grpc-port",
        "-1",
        "-grpc-tls-port",
        "-1",
        "-serf-wan-port",
        "-1",
        "-serf-lan-port",
        "{port1}",
        "-server-port",
        "{port2}",
        "-data-dir",
        "{dir}/data",
    ])
    .extra_ports(2)
    .ready_when_log_matches("Synced node info")
    .startup_timeout(Duration::from_secs(60))
    .start()
    .await
    .expect("start consul agent -dev");
    let addr = agent.addr();
    let (state, id) = client(addr.clone()).await;
    let got = wait_for(&state, id, "consul_kv_get").await;
    assert_eq!(got["result"][0]["value"], "from netget", "{got}");
    assert_eq!(got["result"][0]["flags"], 5);
    let reg = wait_for(&state, id, "consul_register_service").await;
    assert_eq!(reg["status"], 200, "{reg}");
    // The agent's own view, through its own CLI.
    let cli = |args: &[&str]| {
        let out = std::process::Command::new(&bin)
            .args(args)
            .env("CONSUL_HTTP_ADDR", &addr)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(cli(&["kv", "get", "app/config"]), "from netget");
    let detailed = cli(&["kv", "get", "-detailed", "app/config"]);
    assert!(
        detailed
            .lines()
            .any(|l| l.starts_with("Flags") && l.ends_with('5')),
        "{detailed}"
    );
    let services = cli(&["catalog", "services", "-tags"]);
    assert!(
        services
            .lines()
            .any(|l| l.split_whitespace().collect::<Vec<_>>() == ["netget-web", "from-netget"]),
        "{services}"
    );
    // Injected: the catalog and health answers decoded for the handler, a delete, a 404.
    let svc = send(
        &state,
        id,
        json!({"type":"consul_catalog","endpoint":"health","name":"netget-web"}),
    )
    .await;
    assert_eq!(svc["result"][0]["port"], 8080, "{svc}");
    assert_eq!(svc["result"][0]["address"], "127.0.0.7");
    let del = send(
        &state,
        id,
        json!({"type":"consul_kv_delete","key":"app/config"}),
    )
    .await;
    assert_eq!(del["result"], true);
    let gone = send(
        &state,
        id,
        json!({"type":"consul_kv_get","key":"app/config"}),
    )
    .await;
    assert_eq!(gone["status"], 404);
    state.remove_client(id).await;
}
