use super::e2e_test::{logs, start};
use std::time::Duration;

#[tokio::test]
async fn independent_graphyte_emitter_reaches_structured_collector() {
    let (state, id, addr) = start(None, None).await;
    let script = r#"
import graphyte, sys
sender = graphyte.Sender('127.0.0.1', port=int(sys.argv[1]), prefix='demo', raise_send_errors=True)
sender.send('load', 1.25, timestamp=1700000000)
sender.send('温度', -2.5, timestamp=1700000001, tags={'site':'lab'})
sender.stop()
"#;
    let mut command = tokio::process::Command::new(
        std::env::var("NETGET_GRAPHITE_PYTHON").unwrap_or_else(|_| "python3".into()),
    );
    command
        .args(["-c", script, &addr.port().to_string()])
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "Graphyte peer failed; bootstrap pinned peers first: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let entries = logs(&state, id, 2).await;
    let metrics: Vec<_> = entries
        .iter()
        .filter(|e| e.event_type == "graphite_batch")
        .flat_map(|e| e.request["metrics"].as_array().unwrap().clone())
        .collect();
    assert_eq!(metrics.len(), 2);
    assert!(metrics.iter().any(
        |m| m == &serde_json::json!({"path":"demo.load","value":1.25,"timestamp":1700000000.0})
    ));
    assert!(metrics.iter().any(|m|m==&serde_json::json!({"path":"demo.温度;site=lab","value":-2.5,"timestamp":1700000001.0})));
    state.remove_server(id).await;
}
