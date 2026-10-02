use super::e2e_test::{send, start};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn official_carbon_receiver_observes_our_plaintext_metrics() {
    // Use Carbon's actual TCP protocol and metricReceived event. No parser or
    // timestamp conversion is replaced; omit its storage service entirely.
    let script = r#"
import json
from twisted.internet import reactor
from carbon import events, state, instrumentation
from carbon.conf import settings
from carbon.protocols import CarbonReceiverFactory, MetricLineReceiver
state.instrumentation = instrumentation
settings.MIN_TIMESTAMP_RESOLUTION = 0
settings.LOG_LISTENER_CONN_SUCCESS = False
settings.LOG_LISTENER_CONN_LOST = False
settings.TCP_KEEPALIVE = False
settings.USE_FLOW_CONTROL = False
def observed(path, datapoint):
    print('NETGET_METRIC ' + json.dumps({'path':path,'timestamp':datapoint[0],'value':datapoint[1]}), flush=True)
events.metricReceived.addHandler(observed)
factory = CarbonReceiverFactory()
factory.protocol = MetricLineReceiver
port = reactor.listenTCP(0, factory, interface='127.0.0.1')
print('NETGET_PORT ' + str(port.getHost().port), flush=True)
reactor.callLater(30, reactor.stop)
reactor.run()
"#;
    let mut child = tokio::process::Command::new(
        std::env::var("NETGET_GRAPHITE_PYTHON").unwrap_or_else(|_| "python3".into()),
    )
    .args(["-u", "-c", script])
    .stdout(Stdio::piped())
    .stderr(Stdio::inherit())
    .kill_on_drop(true)
    .spawn()
    .expect("Python peer executable");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("Carbon peer exited before listen: install pinned Python3.11 peers");
    let port: u16 = line
        .strip_prefix("NETGET_PORT ")
        .expect("Carbon listening marker")
        .parse()
        .unwrap();
    let (state, id) = start(
        format!("127.0.0.1:{port}"),
        json!({"type":"static","actions":[]}),
    )
    .await;
    assert!(matches!(send(&state,id,json!([{"path":"servers.demo.load","value":1.25,"timestamp":1700000000.25},{"path":"温度;site=lab","value":-2.5,"timestamp":1700000001},{"path":"receiver.clock","value":3,"timestamp":-1}])).await,ClientSendOutcome::Sent{..}));
    let mut got = Vec::new();
    for _ in 0..3 {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("Carbon observation");
        got.push(
            serde_json::from_str::<Value>(
                line.strip_prefix("NETGET_METRIC ")
                    .expect("Carbon metric marker"),
            )
            .unwrap(),
        );
    }
    assert_eq!(
        got[0],
        json!({"path":"servers.demo.load","value":1.25,"timestamp":1700000000.25})
    );
    assert_eq!(
        got[1],
        json!({"path":"温度;site=lab","value":-2.5,"timestamp":1700000001.0})
    );
    assert_eq!(got[2]["path"], "receiver.clock");
    assert!(got[2]["timestamp"].as_f64().unwrap() > 1700000000.0);
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
