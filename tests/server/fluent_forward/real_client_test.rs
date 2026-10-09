use super::e2e_test::{logs, start};
use std::time::Duration;
#[tokio::test]
async fn pinned_fluent_logger_emits_integer_and_eventtime_records() {
    let (state, id, addr) = start(None, None).await;
    let script = r#"
import importlib.metadata
assert importlib.metadata.version('fluent-logger') == '0.11.1'
assert importlib.metadata.version('msgpack') == '1.1.2'
from fluent.sender import FluentSender,EventTime
import sys
s=FluentSender('demo',host='127.0.0.1',port=int(sys.argv[1]),nanosecond_precision=True)
assert s.emit_with_time('logs',1700000000,{'message':'independent','n':42})
assert s.emit_with_time('logs',EventTime(1700000000,nanoseconds=250000000),{'message':'温度','ok':True})
s.close()
"#;
    let mut command = tokio::process::Command::new(
        std::env::var("NETGET_FORWARD_PYTHON").unwrap_or_else(|_| "python3".into()),
    );
    command
        .args(["-c", script, &addr.port().to_string()])
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "fluent-logger peer failed/missing: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let e = logs(&state, id, 2).await;
    assert!(e
        .iter()
        .any(|e| e.request["entries"][0]["record"]["message"] == "independent"));
    let e = e
        .iter()
        .find(|e| e.request["entries"][0]["record"]["message"] == "温度")
        .unwrap();
    assert_eq!(
        e.request["entries"][0]["timestamp"]["nanoseconds"],
        250000000
    );
    state.remove_server(id).await;
}
