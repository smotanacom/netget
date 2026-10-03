use super::e2e_test::{batch, response_logs, send, start};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};

#[tokio::test]
async fn official_influxdb_291_accepts_native_writes_readback_and_real_api_errors() {
    let binary=std::env::var("NETGET_INFLUX_DAEMON").expect("NETGET_INFLUX_DAEMON required on Linux/macOS; run pinned install_daemon.py (Linuxamd64 or existing macOSamd64 support)");
    let out = tokio::process::Command::new(&binary)
        .arg("version")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("2.9.1"),
        "pinned official daemon version"
    );
    let root = tempfile::Builder::new()
        .prefix("netget-influx-service-")
        .tempdir()
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
    let mut child = tokio::process::Command::new(&binary)
        .args(["--http-bind-address", &addr.to_string(), "--bolt-path"])
        .arg(root.path().join("influxd.bolt"))
        .arg("--sqlite-path")
        .arg(root.path().join("influxd.sqlite"))
        .arg("--engine-path")
        .arg(root.path().join("engine"))
        .arg("--reporting-disabled")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = format!("http://{addr}");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if http
                .get(format!("{url}/health"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(root.path().join("daemon.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("official daemon readiness");
    let setup=http.post(format!("{url}/api/v2/setup")).json(&json!({"username":"testuser","password":"owned-test-password","org":"org 名 &","bucket":"bucket / &","retentionPeriodSeconds":0})).send().await.unwrap().error_for_status().unwrap().json::<Value>().await.unwrap();
    let token = setup["auth"]["token"].as_str().expect("setup token");
    let (state, id) = start(url.clone(), None, Some(token)).await;
    for precision in ["ns", "us", "ms", "s"] {
        for gzip in [false, true] {
            assert!(matches!(
                send(&state, id, batch(precision, gzip)).await,
                netget::state::client_handles::ClientSendOutcome::Executed { .. }
            ));
        }
    }
    let entries = response_logs(&state, id, 8).await;
    assert!(entries.iter().all(|e| e.request["status"] == 204));
    // Read back with the independently maintained official Python client. The production
    // implementation remains write-only; query is only an oracle against the real service.
    let script = r#"
import importlib.metadata,json,sys
assert importlib.metadata.version('influxdb-client')=='1.50.0'
from influxdb_client import InfluxDBClient
with InfluxDBClient(url=sys.argv[1],token=sys.stdin.read(),org='org 名 &',timeout=5000) as c:
 query='from(bucket: "bucket / &") |> range(start: time(v: 0), stop: time(v: 2000000000000)) |> map(fn: (r) => ({r with ts_ns: int(v: r._time)}))'
 records=[r for t in c.query_api().query(query) for r in t.records]
 assert len(records)==20,len(records)
 expect={'float':1.25,'int':-42,'uint':18446744073709551615,'bool':True,'str =,':'say "hi" \\ literal\\n\t名'}
 assert {r.values['ts_ns'] for r in records}=={123,123000,123000000,123000000000}
 for r in records:
  assert r.get_measurement()=='温 度,'
  assert r.values['host =,']=='a,b =名'
  assert r.get_value()==expect[r.get_field()],(r.get_field(),r.get_value())
 print('NETGET_DAEMON_READBACK 20')
"#;
    use tokio::io::AsyncWriteExt;
    let mut python = tokio::process::Command::new(
        std::env::var("NETGET_INFLUX_PYTHON").expect("official Python peer required"),
    )
    .args(["-c", script, &url])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    python
        .stdin
        .take()
        .unwrap()
        .write_all(token.as_bytes())
        .await
        .unwrap();
    let out = tokio::time::timeout(Duration::from_secs(15), python.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "official daemon readback: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("NETGET_DAEMON_READBACK 20"));
    let mut missing = batch("ns", false);
    missing["bucket"] = json!("missing-bucket");
    send(&state, id, missing).await;
    assert_eq!(response_logs(&state, id, 9).await[8].request["status"], 404);
    state.remove_client(id).await;
    let (state, id) = start(url, None, Some("wrong-token")).await;
    send(&state, id, batch("ns", true)).await;
    let e = response_logs(&state, id, 1).await;
    assert_eq!(e[0].request["status"], 401);
    assert_eq!(e[0].request["error"]["code"], "unauthorized");
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
