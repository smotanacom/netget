//! Independent LPD clients against NetGet's server, failing rather than skipping when absent:
//! LPRng 3.8.B's lpr, lpq and lprm, and CUPS's lpd backend, both unchanged.
//! `scripts/test-peers/install-lprng.sh` installs them and prints the environment this reads.
//!
//! The job handler records every event it is given, so what each client actually put in its
//! control and data files is asserted from the server's side, and it accepts only documents
//! containing ACCEPT, so each client's handling of a refused job is exercised too.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn lprng(tool: &str) -> PathBuf {
    if let Some(root) = std::env::var_os("NETGET_LPRNG_ROOT") {
        return PathBuf::from(root).join("usr/bin").join(tool);
    }
    let found = crate::helpers::real_server::find_binary(tool)
        .unwrap_or_else(|| panic!("LPRng {tool} is required: run scripts/test-peers/install-lprng.sh and set NETGET_LPRNG_ROOT"));
    let version = std::process::Command::new(&found)
        .arg("-V")
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&version.stdout).contains("LPRng")
            || String::from_utf8_lossy(&version.stderr).contains("LPRng"),
        "{} is not LPRng's (CUPS ships an IPP-only {tool}); set NETGET_LPRNG_ROOT",
        found.display()
    );
    found
}

fn recording_script(record: &Path) -> String {
    format!(
        "import json,sys\ne=json.load(sys.stdin)['event']\nopen({path:?},'a').write(json.dumps(e)+'\\n')\nok=any('ACCEPT' in (f['text'] or '') for f in e['files'])\nprint(json.dumps({{'actions':[{{'type':'lpd_job_reply','accept':ok}}]}}))",
        path = record.to_str().unwrap()
    )
}

fn recorded(record: &Path) -> Vec<Value> {
    std::fs::read_to_string(record)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

async fn run(program: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut command = tokio::process::Command::new(program);
    command.args(args).kill_on_drop(true);
    for (k, v) in env {
        command.env(k, v);
    }
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .unwrap_or_else(|_| panic!("{} deadline", program.display()))
        .unwrap_or_else(|e| panic!("start {}: {e}", program.display()))
}

#[tokio::test]
async fn lprng_lpr_lpq_and_lprm() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("jobs.jsonl");
    let (state, id, addr) = start(
        handlers(&recording_script(&record)),
        json!({"queues":["raw"]}),
    )
    .await;
    let printer = format!("raw@127.0.0.1%{}", addr.port());
    let accepted = dir.path().join("report.txt");
    std::fs::write(&accepted, "ACCEPT this page\n").unwrap();
    let out = run(
        &lprng("lpr"),
        &[
            "-P",
            &printer,
            "-J",
            "quarterly",
            "-C",
            "B",
            accepted.to_str().unwrap(),
        ],
        &[],
    )
    .await;
    assert!(
        out.status.success(),
        "lpr refused an accepted job: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let refused = dir.path().join("refused.txt");
    std::fs::write(&refused, "please refuse\n").unwrap();
    let out = run(
        &lprng("lpr"),
        &["-P", &printer, "-J", "nope", refused.to_str().unwrap()],
        &[],
    )
    .await;
    assert!(
        !out.status.success(),
        "lpr must report a refused job as a failure"
    );
    let jobs = recorded(&record);
    let job = jobs
        .iter()
        .find(|j| j["job_name"] == "quarterly")
        .unwrap_or_else(|| panic!("{jobs:?}"));
    assert_eq!(job["queue"], "raw");
    assert_eq!(job["class"], "B");
    assert!(job["user"].as_str().is_some_and(|u| !u.is_empty()), "{job}");
    assert_eq!(job["files"][0]["text"], "ACCEPT this page\n", "{job}");
    assert_eq!(job["files"][0]["size"], 17, "{job}");
    assert!(
        job["files"][0]["source_name"]
            .as_str()
            .unwrap()
            .ends_with("report.txt"),
        "{job}"
    );
    assert!(jobs.iter().any(|j| j["job_name"] == "nope"), "{jobs:?}");

    let out = run(&lprng("lpq"), &["-P", &printer], &[]).await;
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(listing.contains("raw is ready and printing"), "{listing}");
    assert!(
        listing.contains("alice") && listing.contains("report.txt") && listing.contains("1024"),
        "{listing}"
    );
    let out = run(&lprng("lprm"), &["-P", &printer, "42"], &[]).await;
    let removed =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(removed.contains("job 042 dequeued"), "{removed}");
    state.remove_server(id).await;
}

#[tokio::test]
async fn cups_lpd_backend_in_both_file_orders() {
    let backend = std::env::var_os("NETGET_CUPS_LPD_BACKEND")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/usr/lib/cups/backend/lpd")).filter(|p| p.exists()))
        .expect("CUPS lpd backend is required: run scripts/test-peers/install-lprng.sh and set NETGET_CUPS_LPD_BACKEND");
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("jobs.jsonl");
    let (state, id, addr) = start(
        handlers(&recording_script(&record)),
        json!({"queues":["raw"]}),
    )
    .await;
    let document = dir.path().join("doc.txt");
    std::fs::write(&document, "ACCEPT from CUPS\n").unwrap();
    for (order, title) in [
        ("control,data", "cups-control-first"),
        ("data,control", "cups-data-first"),
    ] {
        let uri = format!(
            "lpd://127.0.0.1:{}/raw?reserve=none&banner=off&format=l&order={order}&contimeout=10",
            addr.port()
        );
        let out = run(
            &backend,
            &["17", "alice", title, "1", "", document.to_str().unwrap()],
            &[("DEVICE_URI", &uri)],
        )
        .await;
        assert!(
            out.status.success(),
            "CUPS lpd backend ({order}) failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let jobs = recorded(&record);
    for title in ["cups_control_first", "cups_data_first"] {
        let job = jobs
            .iter()
            .find(|j| j["job_name"] == title || j["files"][0]["source_name"] == title)
            .unwrap_or_else(|| panic!("no {title} job in {jobs:?}"));
        assert_eq!(job["user"], "alice", "{job}");
        assert_eq!(job["files"][0]["format"], "l", "{job}");
        assert_eq!(job["files"][0]["text"], "ACCEPT from CUPS\n", "{job}");
    }
    state.remove_server(id).await;
}
