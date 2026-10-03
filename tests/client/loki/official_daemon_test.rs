use super::e2e_test::{batch, response_logs, send, start};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
fn port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}
#[tokio::test]
async fn official_loki_378_reads_back_all_carriers_metadata_tenants_and_real_errors() {
    let binary = std::env::var("NETGET_LOKI_PEER")
        .expect("NETGET_LOKI_PEER required; pinned install_peers.py, no missing-service skip");
    let version = tokio::process::Command::new(&binary)
        .arg("-version")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("3.7.8"));
    let root = tempfile::Builder::new()
        .prefix("netget-loki-service-")
        .tempdir()
        .unwrap();
    let http_port = port();
    let grpc_port = port();
    let path = root.path().display();
    let config=format!("auth_enabled: true\nserver:\n  http_listen_address: 127.0.0.1\n  http_listen_port: {http_port}\n  grpc_listen_address: 127.0.0.1\n  grpc_listen_port: {grpc_port}\ncommon:\n  instance_addr: 127.0.0.1\n  path_prefix: {path}\n  storage:\n    filesystem:\n      chunks_directory: {path}/chunks\n      rules_directory: {path}/rules\n  replication_factor: 1\n  ring:\n    kvstore:\n      store: inmemory\ningester:\n  wal:\n    enabled: false\nschema_config:\n  configs:\n    - from: 2020-01-01\n      store: tsdb\n      object_store: filesystem\n      schema: v13\n      index:\n        prefix: index_\n        period: 24h\nlimits_config:\n  reject_old_samples: false\n  allow_structured_metadata: true\n  discover_service_name: []\n  discover_log_levels: false\nanalytics:\n  reporting_enabled: false\n");
    std::fs::write(root.path().join("config.yaml"), config).unwrap();
    let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
    let mut child = tokio::process::Command::new(binary)
        .arg(format!("-config.file={path}/config.yaml"))
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let url = format!("http://127.0.0.1:{http_port}");
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    // Probe readiness with a separate label stream. This never retries a production-client
    // operation and catches an HTTP-ready service whose ingester ring is not active yet.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64;
    tokio::time::timeout(Duration::from_secs(45),async{loop{assert!(child.try_wait().unwrap().is_none(),"daemon exit: {}",std::fs::read_to_string(root.path().join("daemon.log")).unwrap());let r=http.post(format!("{url}/loki/api/v1/push")).header("X-Scope-OrgID","probe").json(&json!({"streams":[{"stream":{"app":"readiness"},"values":[[now.to_string(),"ready"]]}]})).send().await;if r.is_ok_and(|r|r.status().as_u16()==204){
 let query=http.get(format!("{url}/loki/api/v1/query_range")).header("X-Scope-OrgID","probe").query(&[("query","{app=\"readiness\"}".to_string()),("start",(now-1).to_string()),("end",(now+1).to_string())]).send().await;
 if let Ok(response)=query {if response.status().is_success(){if let Ok(value)=response.json::<Value>().await{if value["data"]["result"].as_array().is_some_and(|a|!a.is_empty()){break;}}}}
}tokio::time::sleep(Duration::from_millis(50)).await;}}).await.unwrap_or_else(|_|panic!("official daemon ingestion/query readiness: {}",std::fs::read_to_string(root.path().join("daemon.log")).unwrap()));
    let (state, id) = start(url.clone(), None, None).await;
    for (i, encoding) in ["json", "gzip_json", "snappy_protobuf"]
        .into_iter()
        .enumerate()
    {
        let mut b = batch(encoding);
        for (j, e) in b["streams"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            e["timestamp_ns"] = json!(now + (i * 2 + j) as i64);
        }
        send(&state, id, b).await;
    }
    let events = response_logs(&state, id, 3).await;
    assert!(
        events.iter().all(|e| e.request["status"] == 204),
        "{events:?}"
    );
    let readback = http
        .get(format!("{url}/loki/api/v1/query_range"))
        .header("X-Scope-OrgID", "tenant-one")
        .query(&[
            ("query", "{app=\"sample 名\"}".to_string()),
            ("start", now.to_string()),
            ("end", (now + 6).to_string()),
            ("direction", "forward".into()),
        ])
        .send()
        .await
        .unwrap();
    let status = readback.status();
    let text = readback.text().await.unwrap();
    assert!(status.is_success(), "query error: {text}");
    let result: Value = serde_json::from_str(&text).unwrap();
    let streams = result["data"]["result"].as_array().unwrap();
    // Loki groups query results by labels including returned metadata. Compare every
    // entry across those groups, preserving the exact requested timestamp/line/meta oracle.
    let mut values = Vec::new();
    for stream in streams {
        assert_eq!(stream["stream"]["app"], "sample 名");
        assert_eq!(stream["stream"]["host"], "say \"hi\" \\ \n\t");
        for value in stream["values"].as_array().unwrap() {
            values.push((value.clone(), stream["stream"].clone()));
        }
    }
    values.sort_by_key(|(value, _)| value[0].as_str().unwrap().parse::<i64>().unwrap());
    assert_eq!(values.len(), 6, "{result}");
    for (i, (value, labels)) in values.iter().enumerate() {
        assert_eq!(value[0], (now + i as i64).to_string());
        assert_eq!(
            value[1],
            if i % 2 == 0 {
                "entry 名 \"quote\" \n\t"
            } else {
                "second line"
            }
        );
        if i % 2 == 0 {
            let metadata = if value.get(2).is_some() {
                &value[2]
            } else {
                labels
            };
            assert_eq!(metadata["trace_id"], "0123名");
            assert_eq!(metadata["user_id"], "two");
        }
    }
    let other = http
        .get(format!("{url}/loki/api/v1/query_range"))
        .header("X-Scope-OrgID", "other")
        .query(&[
            ("query", "{app=\"sample 名\"}"),
            ("start", &now.to_string()),
            ("end", &(now + 6).to_string()),
        ])
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(other["data"]["result"], json!([]));
    let mut b = batch("json");
    b.as_object_mut().unwrap().remove("tenant_id");
    send(&state, id, b).await;
    assert_eq!(response_logs(&state, id, 4).await[3].request["status"], 401);
    let mut b = batch("snappy_protobuf");
    b["streams"][0]["entries"][0]["timestamp_ns"] = json!(i64::MAX);
    b["streams"][0]["entries"]
        .as_array_mut()
        .unwrap()
        .truncate(1);
    send(&state, id, b).await;
    let e = response_logs(&state, id, 5).await;
    assert_eq!(e[4].request["status"], 400);
    let message = e[4].request["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("timestamp too new"),
        "official future-only error: {message}"
    );
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
