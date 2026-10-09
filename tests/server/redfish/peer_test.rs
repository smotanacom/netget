//! Two independent Redfish clients, unchanged, against NetGet's service: gofish v0.26.0 (Go,
//! session login, typed parsing of every resource it reads, a reset that returns a task it
//! then polls, a PATCH, chassis sensors, managers, sessions, logout) and DMTF redfishtool
//! 1.1.8 (Python, Basic and session auth, a reset it waits on). Both are refused a wrong
//! password. Fails, never skips.
use crate::helpers::redfish::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};
use std::time::Duration;

async fn run(program: &str, args: &[&str]) -> (bool, String, String) {
    let out = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(program)
            .args(args)
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
async fn gofish_logs_in_walks_resets_and_patches() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        bmc_policy(&dir.path().join("bmc.json")),
        json!({"product": "NetGet BMC"}),
    )
    .await;
    let (ok, stdout, stderr) =
        run(&gofish(), &[&format!("http://{addr}"), "admin", "secret"]).await;
    assert!(ok, "gofish failed:\n{stdout}\n{stderr}");
    let steps: std::collections::HashMap<String, Value> = stdout
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .map(|v| (v["step"].as_str().unwrap().to_owned(), v))
        .collect();
    assert_eq!(steps["bad_login"]["refused"], true);
    assert_eq!(
        (
            steps["root"]["redfish_version"].as_str(),
            steps["root"]["product"].as_str()
        ),
        (Some("1.20.0"), Some("NetGet BMC"))
    );
    assert_eq!(
        steps["systems"]["systems"],
        json!([{"id": "1", "name": "NetGet Server", "power": "On", "model": "NG-1", "cpus": 2, "memory_gib": 64, "reset_types": ["On", "ForceOff", "ForceRestart", "GracefulShutdown"], "asset_tag": ""}])
    );
    assert_eq!(steps["reset"]["state"], "Running");
    assert!(steps["reset"]["monitor"]
        .as_str()
        .unwrap()
        .contains("/TaskService/TaskMonitors/"));
    assert_eq!(
        (
            steps["task"]["state"].as_str(),
            steps["task"]["percent"].as_u64()
        ),
        (Some("Completed"), Some(100))
    );
    assert_eq!(steps["patch"]["asset_tag"], "netget-asset-1");
    assert_eq!(
        steps["chassis"]["chassis"],
        json!([{"id": "1", "type": "RackMount", "sensors": [
            {"id": "CPU1Temp", "reading": 42.5, "units": "Cel", "type": "Temperature"},
            {"id": "InletTemp", "reading": 21, "units": "Cel", "type": "Temperature"}]}])
    );
    assert_eq!(
        steps["managers"]["managers"],
        json!([{"id": "bmc", "type": "BMC", "firmware": "1.0.0"}])
    );
    assert_eq!(
        (
            steps["sessions"]["count"].as_u64(),
            steps["sessions"]["timeout"].as_u64()
        ),
        (Some(1), Some(1800))
    );
    assert!(steps.contains_key("logout"));
    let owner = AccessLogOwner::Server(sid.as_u32());
    let logins = logs(&state, owner, "redfish_login", 2).await;
    assert_eq!(logins[0].request["password"], "wrong");
    let acts = logs(&state, owner, "redfish_request", 1).await;
    let reset = acts
        .iter()
        .find(|r| r.request["kind"] == "action")
        .expect("the reset reached the handler");
    assert_eq!(
        (
            reset.request["action"].as_str(),
            reset.request["body"]["ResetType"].as_str()
        ),
        (Some("ComputerSystem.Reset"), Some("ForceRestart"))
    );
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn redfishtool_reads_and_resets_with_basic_and_session_auth() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(&state, bmc_policy(&dir.path().join("bmc.json")), json!({})).await;
    let host = addr.to_string();
    let base = [
        "-r",
        host.as_str(),
        "-S",
        "Never",
        "-u",
        "admin",
        "-p",
        "secret",
    ];
    let tool = |extra: &'static [&'static str]| {
        let mut a: Vec<&str> = base.to_vec();
        a.extend_from_slice(extra);
        a
    };
    let (ok, out, err) = run(
        &redfishtool(),
        &tool(&["-A", "Basic", "Systems", "-I", "1"]),
    )
    .await;
    assert!(ok, "{out}\n{err}");
    let system: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        (system["Id"].as_str(), system["PowerState"].as_str()),
        (Some("1"), Some("On"))
    );
    let (ok, out, err) = run(
        &redfishtool(),
        &tool(&[
            "-A",
            "Basic",
            "Systems",
            "-I",
            "1",
            "reset",
            "GracefulShutdown",
        ]),
    )
    .await;
    assert!(ok, "reset failed:\n{out}\n{err}");
    let (ok, out, err) = run(
        &redfishtool(),
        &tool(&[
            "-A",
            "Session",
            "raw",
            "GET",
            "/redfish/v1/Chassis/1/Sensors",
        ]),
    )
    .await;
    assert!(ok, "{out}\n{err}");
    let sensors: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(sensors["Members@odata.count"], 2);
    let (ok, out, err) = run(
        &redfishtool(),
        &tool(&["-A", "Session", "Managers", "-I", "bmc"]),
    )
    .await;
    assert!(ok, "{out}\n{err}");
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["ManagerType"],
        "BMC"
    );
    let (ok, _, _) = run(
        &redfishtool(),
        &[
            "-r",
            host.as_str(),
            "-S",
            "Never",
            "-u",
            "admin",
            "-p",
            "nope",
            "-A",
            "Basic",
            "Systems",
            "-I",
            "1",
        ],
    )
    .await;
    assert!(!ok, "redfishtool must be refused a wrong password");
    let owner = AccessLogOwner::Server(sid.as_u32());
    let acts = logs(&state, owner, "redfish_request", 1).await;
    assert!(
        acts.iter()
            .any(|r| r.request["body"]["ResetType"] == "GracefulShutdown"),
        "the reset reached the handler"
    );
    let logins = logs(&state, owner, "redfish_login", 3).await;
    assert!(logins.iter().any(|l| l.request["method"] == "session"));
    assert!(logins.iter().any(|l| l.request["method"] == "basic"));
    state.remove_server(sid).await;
}
