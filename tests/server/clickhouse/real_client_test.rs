//! Independent ClickHouse clients against NetGet's server, failing rather than skipping when
//! absent: the official clickhouse-client 24.8 (C++), uncompressed and with --compression 1,
//! and Python clickhouse-driver 0.2.11. Each reads the typed result set, runs an INSERT whose
//! rows the handler sees, and reports the handler's exception.
//! `tests/client/clickhouse/install_peers.py` prints NETGET_CLICKHOUSE_BIN and
//! NETGET_CLICKHOUSE_PYTHON.
use super::wire_test::{handler_saw, handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/clickhouse/install_peers.py <root> and export what it prints"))
}

async fn client(port: u16, extra: &[&str], query: &str) -> (bool, String) {
    let mut args: Vec<String> = [
        "client",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--user",
        "analyst",
        "--password",
        "s3cret",
        "--connect_timeout",
        "10",
        "--receive_timeout",
        "30",
        "--send_timeout",
        "30",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend(["--query".into(), query.into()]);
    let mut child = tokio::process::Command::new(env_path("NETGET_CLICKHOUSE_BIN"))
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start clickhouse-client");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (a, b) = (collected.clone(), collected.clone());
    let out_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut buf).await;
        a.lock().unwrap().extend(buf);
    });
    let err_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buf).await;
        b.lock().unwrap().extend(buf);
    });
    let status = match tokio::time::timeout(Duration::from_secs(60), child.wait()).await {
        Ok(status) => status.expect("wait for clickhouse-client"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = out_task.await;
            let _ = err_task.await;
            panic!(
                "clickhouse-client {args:?} did not finish in 60 s; it printed: {}",
                String::from_utf8_lossy(&collected.lock().unwrap())
            );
        }
    };
    let _ = out_task.await;
    let _ = err_task.await;
    let text = String::from_utf8_lossy(&collected.lock().unwrap()).to_string();
    (status.success(), text)
}

#[tokio::test]
async fn official_clickhouse_client() {
    let (state, id, addr) =
        start(handlers(), json!({"user": "analyst", "password": "s3cret"})).await;
    for compression in ["0", "1"] {
        let (ok, out) = client(
            addr.port(),
            &["--compression", compression, "--format", "JSONCompact"],
            "SELECT * FROM events",
        )
        .await;
        assert!(ok, "{out}");
        let doc: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
        assert_eq!(
            doc["data"],
            json!([
                [
                    1,
                    "alpha",
                    1.5,
                    "2024-01-02",
                    "2024-01-02 03:04:05",
                    true,
                    "-7"
                ],
                [
                    2,
                    "beta",
                    null,
                    "2024-12-31",
                    "2024-12-31 23:59:59",
                    false,
                    "9007199254740993"
                ]
            ]),
            "compression {compression}"
        );
        assert_eq!(doc["meta"][4], json!({"name": "at", "type": "DateTime"}));
    }
    let (ok, out) = client(
        addr.port(),
        &[],
        "INSERT INTO events (id, name) VALUES (10, 'from-client'), (11, 'second')",
    )
    .await;
    assert!(ok, "{out}");
    assert!(handler_saw(&state, id, r#""rows":[[10,"from-client"],[11,"second"]]"#).await);
    let (ok, out) = client(addr.port(), &[], "SELECT * FROM missing").await;
    assert!(
        !ok && out.contains("Code: 60") && out.contains("Table default.missing does not exist"),
        "{out}"
    );
    state.remove_server(id).await;
}

const DRIVER: &str = r#"import json, sys
from clickhouse_driver import Client
from clickhouse_driver.errors import ServerException
c = Client('127.0.0.1', port=int(sys.argv[1]), user='analyst', password='s3cret')
rows = c.execute('SELECT * FROM events', settings={'max_threads': 2})
out = {'rows': [[str(v) if v is not None else None for v in r] for r in rows]}
out['inserted'] = c.execute('INSERT INTO events (id, name) VALUES', [(20, 'from-driver')])
try:
    c.execute('SELECT * FROM missing')
except ServerException as e:
    out['code'] = e.code
c.disconnect()
print(json.dumps(out))"#;

#[tokio::test]
async fn python_clickhouse_driver() {
    let (state, id, addr) =
        start(handlers(), json!({"user": "analyst", "password": "s3cret"})).await;
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(env_path("NETGET_CLICKHOUSE_PYTHON"))
            .args(["-I", "-c", DRIVER, &addr.port().to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("driver deadline")
    .expect("start python");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let doc: Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(
        doc,
        json!({
            "rows": [["1", "alpha", "1.5", "2024-01-02", "2024-01-02 03:04:05", "True", "-7"], ["2", "beta", null, "2024-12-31", "2024-12-31 23:59:59", "False", "9007199254740993"]],
            "inserted": 1,
            "code": 60,
        })
    );
    assert!(handler_saw(&state, id, r#""rows":[[20,"from-driver"]]"#).await);
    assert!(
        handler_saw(&state, id, r#""max_threads":"2""#).await,
        "settings reach the handler"
    );
    state.remove_server(id).await;
}
