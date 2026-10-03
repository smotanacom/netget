use super::e2e_test::{logs, start, TARGET};
use serde_json::json;
use std::{process::Stdio, time::Duration};
#[tokio::test]
async fn independent_python_json_writer_preserves_labels_unicode_and_observes_rejection() {
    let (state, id, addr) = start(
        None,
        Some(json!({"auth_token":"peer-secret","require_tenant":true})),
    )
    .await;
    let python = std::env::var("NETGET_LOKI_PYTHON")
        .expect("pinned independent Python JSON writer required; install_peers.py");
    let script = r#"
import importlib.metadata as m,logging,sys
assert m.version('python-logging-loki')=='0.3.1'
from logging_loki.emitter import LokiEmitterV1
with_emitter=LokiEmitterV1(sys.argv[1],tags={'app':'python-peer','host':'say "hi" \\ 名'})
with_emitter.session.trust_env=False
with_emitter.session.headers.update({'X-Scope-OrgID':'tenant-one','Authorization':'Bearer peer-secret'})
r=logging.LogRecord('independent.logger',logging.INFO,'',1,'',(),None)
try:
 with_emitter(r,'hello 名 "quote"\nsecond')
except ValueError as e:
 assert sys.argv[2]=='400' and '400' in str(e),str(e)
 print('NETGET_JSON_REJECTION 400')
else:
 assert sys.argv[2]=='204'
 print('NETGET_JSON_ACCEPT 204')
finally:with_emitter.close()
"#;
    let url = format!("http://{addr}{TARGET}");
    for status in [204, 400] {
        if status == 400 {
            state.set_event_handler_config(id,Some(serde_json::from_value(json!({"handlers":[{"event_pattern":"loki_push","handler":{"type":"static","actions":[{"type":"reject_loki_entries","status":400,"message":"schema decision"}]}}]})).unwrap())).await;
        }
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(&python)
                .args(["-c", script, &url, &status.to_string()])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains(&status.to_string()));
        let e = logs(&state, id, if status == 204 { 1 } else { 2 }).await;
        let event = e.iter().find(|e| e.event_type == "loki_push").unwrap();
        assert_eq!(event.request["encoding"], "json");
        assert_eq!(event.request["tenant_id"], "tenant-one");
        let stream = &event.request["streams"][0];
        assert_eq!(stream["labels"]["app"], "python-peer");
        assert_eq!(stream["labels"]["host"], "say \"hi\" \\ 名");
        assert_eq!(stream["labels"]["severity"], "info");
        assert_eq!(stream["labels"]["logger"], "independent.logger");
        assert_eq!(stream["entries"][0]["line"], "hello 名 \"quote\"\nsecond");
        assert!(stream["entries"][0]["timestamp_ns"].as_i64().unwrap() > 1700000000000000000);
    }
    state.remove_server(id).await;
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn maintained_alloy_writer_sends_snappy_protobuf_metadata_tenant_auth_and_observes_400() {
    let binary = std::env::var("NETGET_ALLOY_PEER")
        .expect("NETGET_ALLOY_PEER required; pinned install_peers.py; no missing-writer skip");
    let version = tokio::process::Command::new(&binary)
        .arg("--version")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("1.20.1"));
    let (state, id, addr) = start(
        None,
        Some(json!({"require_tenant":true,"auth_token":"peer-secret"})),
    )
    .await;
    let root = tempfile::Builder::new()
        .prefix("netget-alloy-writer-")
        .tempdir()
        .unwrap();
    let port = || {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let input = port();
    let control = port();
    let config = format!(
        r#"loki.write "out" {{
 endpoint {{
  url = "http://{addr}/loki/api/v1/push"
  tenant_id = "tenant-one"
  bearer_token = "peer-secret"
  batch_wait = "50ms"
  batch_size = "16KiB"
  min_backoff_period = "50ms"
  max_backoff_period = "100ms"
  max_backoff_retries = 1
  retry_on_http_429 = false
  enable_http2 = false
  follow_redirects = false
 }}
}}
loki.source.api "in" {{
 http {{
  listen_address = "127.0.0.1"
  listen_port = {input}
 }}
 use_incoming_timestamp = true
 forward_to = [loki.write.out.receiver]
}}
"#
    );
    std::fs::write(root.path().join("config.alloy"), config).unwrap();
    let log = std::fs::File::create(root.path().join("alloy.log")).unwrap();
    let mut child = tokio::process::Command::new(binary)
        .arg("run")
        .arg("--disable-reporting")
        .arg(format!("--server.http.listen-addr=127.0.0.1:{control}"))
        .arg("--server.http.enable-pprof=false")
        .arg("--storage.path")
        .arg(root.path().join("data"))
        .arg(root.path().join("config.alloy"))
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{input}");
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if http
                .get(format!("{url}/ready"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "Alloy exited: {}",
                std::fs::read_to_string(root.path().join("alloy.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let wire = json!({"streams":[{"stream":{"app":"alloy-peer","host":"say \"hi\" \\ 名"},"values":[["1700000000000000123","first 名",{"trace_id":"0123名","user_id":"two"}],["1700000000000000124","second\nline"]]}]});
    assert_eq!(
        http.post(format!("{url}{TARGET}"))
            .header("X-Scope-OrgID", "tenant-one")
            .json(&wire)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        204
    );
    let e = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let e = state
                .list_access_logs_for(
                    Some(netget::state::AccessLogOwner::Server(id.as_u32())),
                    None,
                )
                .await;
            if !e.is_empty() {
                break e;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "Alloy forwarding failed: {}",
            std::fs::read_to_string(root.path().join("alloy.log")).unwrap()
        )
    });
    let data = &e[0].request;
    assert_eq!(data["encoding"], "snappy_protobuf");
    assert_eq!(data["tenant_id"], "tenant-one");
    assert_eq!(data["auth_required"], true);
    let stream = &data["streams"][0];
    assert_eq!(stream["labels"]["app"], "alloy-peer");
    assert_eq!(stream["labels"]["host"], "say \"hi\" \\ 名");
    let entries = stream["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0],
        json!({"timestamp_ns":1700000000000000123i64,"line":"first 名","structured_metadata":{"trace_id":"0123名","user_id":"two"}})
    );
    assert_eq!(
        entries[1],
        json!({"timestamp_ns":1700000000000000124i64,"line":"second\nline","structured_metadata":{}})
    );
    state.set_event_handler_config(id,Some(serde_json::from_value(json!({"handlers":[{"event_pattern":"loki_push","handler":{"type":"static","actions":[{"type":"reject_loki_entries","status":400,"message":"owned rejection"}]}}]})).unwrap())).await;
    let rejected = json!({"streams":[{"stream":{"app":"alloy-reject"},"values":[["1700000000000000125","rejected"]]}]});
    assert!(http
        .post(format!("{url}{TARGET}"))
        .json(&rejected)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    logs(&state, id, 2).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let log = std::fs::read_to_string(root.path().join("alloy.log")).unwrap();
            if log.contains("400") && log.contains("owned rejection") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Alloy must observe native HTTP400 rejection");
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    state.remove_server(id).await;
}
