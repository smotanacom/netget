//! Two independent RDAP clients against NetGet's server, unchanged: OpenRDAP 0.10.2 (Go) and
//! ICANN's rdap 1.0.0 (Rust, built separately from its own lockfile). Lookups of every object
//! class, a search, help, a 404, a 403 error object and a malformed query that never reaches
//! the handler. Both fail, never skip, when absent.
use crate::helpers::rdap::*;
use netget::state::AccessLogOwner;
use serde_json::Value;
use std::time::Duration;

async fn run(program: std::path::PathBuf, args: &[&str]) -> (bool, String, String) {
    let home = tempfile::tempdir().unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(program)
            .args(args)
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer timed out")
    .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn openrdap_reads_every_object_class_search_and_help() {
    let state = state();
    let (sid, addr) = server_in(
        &state,
        registry_policy(),
        serde_json::json!({"base_path": "/rdap"}),
    )
    .await;
    let server = format!("http://{addr}/rdap");
    for (kind, value, expect) in [
        ("domain", "example.com", "EX-1"),
        ("nameserver", "ns1.example.com", "NS-1"),
        ("ip", "192.0.2.7", "NET-1"),
        ("autnum", "64496", "AS64496"),
        ("entity", "EX-REG", "registrant"),
        ("domain-search", "exa*.com", "EX-1"),
    ] {
        let (ok, stdout, stderr) = run(
            openrdap(),
            &["-s", &server, "-t", kind, "--json", "--cache-dir=", value],
        )
        .await;
        assert!(ok, "openrdap {kind} {value} failed: {stdout}{stderr}");
        let parsed: Value =
            serde_json::from_str(&stdout).unwrap_or_else(|_| panic!("{kind}: not JSON: {stdout}"));
        assert!(
            parsed.to_string().contains(expect),
            "{kind} {value}: {stdout}"
        );
    }
    let (ok, stdout, stderr) = run(
        openrdap(),
        &["-s", &server, "-t", "help", "--json", "--cache-dir="],
    )
    .await;
    assert!(ok && stdout.contains("A test registry"), "{stdout}{stderr}");
    let (ok, stdout, stderr) = run(
        openrdap(),
        &[
            "-s",
            &server,
            "-t",
            "domain",
            "--cache-dir=",
            "nope.example",
        ],
    )
    .await;
    assert!(!ok, "a 404 must fail the lookup: {stdout}{stderr}");
    let queries = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "rdap_query",
        8,
    )
    .await;
    let ip = queries
        .iter()
        .find(|q| q.request["query_type"] == "ip")
        .unwrap();
    assert_eq!(ip.request["value"], "192.0.2.7");
    let search = queries
        .iter()
        .find(|q| q.request["query_type"] == "domains")
        .unwrap();
    assert_eq!(
        (
            search.request["search_parameter"].as_str(),
            search.request["value"].as_str()
        ),
        (Some("name"), Some("exa*.com"))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn icann_rdap_reads_objects_and_reports_errors() {
    let state = state();
    let (sid, addr) = server_in(&state, registry_policy(), serde_json::json!({})).await;
    let base = format!("http://{addr}/");
    for (kind, value, expect) in [
        ("domain", "example.com", "\"ldhName\":\"example.com\""),
        ("ns", "ns1.example.com", "\"handle\":\"NS-1\""),
        ("v4-cidr", "192.0.2.0/24", "\"handle\":\"NET-1\""),
        ("autnum", "AS64496", "\"handle\":\"AS64496\""),
        ("entity", "EX-REG", "\"handle\":\"EX-REG\""),
        ("domain-name", "exa*.com", "domainSearchResults"),
    ] {
        let (ok, stdout, stderr) = run(
            icann_rdap(),
            &["-B", &base, "-T", "-N", "-O", "json", "-t", kind, value],
        )
        .await;
        assert!(ok, "icann rdap {kind} {value} failed: {stdout}{stderr}");
        let compact = serde_json::from_str::<Value>(stdout.trim())
            .map(|v| v.to_string())
            .unwrap_or(stdout.clone());
        assert!(compact.contains(expect), "{kind} {value}: {stdout}");
        assert!(
            compact.contains("rdap_level_0"),
            "{kind}: Rust adds rdap_level_0: {stdout}"
        );
    }
    let (ok, stdout, stderr) = run(
        icann_rdap(),
        &[
            "-B", &base, "-T", "-N", "-O", "json", "-t", "autnum", "AS64511",
        ],
    )
    .await;
    assert!(
        !ok || stdout.contains("403"),
        "the 403 error object must reach the client: {stdout}{stderr}"
    );
    let server = AccessLogOwner::Server(sid.as_u32());
    let before = logs(&state, server, "rdap_query", 7).await.len();
    // A malformed path is a 400 and costs no handler call.
    let response = reqwest::get(format!("http://{addr}/autnum/notanumber"))
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(response.headers()["content-type"], "application/rdap+json");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["errorCode"], 400);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = state
        .list_access_logs_for(Some(server), None)
        .await
        .into_iter()
        .filter(|e| e.event_type == "rdap_query")
        .count();
    assert_eq!(before, after);
    state.remove_server(sid).await;
}
