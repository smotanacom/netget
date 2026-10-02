//! StatsD's reference Node collector parses and aggregates NetGet emitter traffic.
//! Requires statsd@0.9.0 (`npm install statsd@0.9.0`, NODE_PATH if installed elsewhere).
use super::e2e_test::{send, start};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::json;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

#[tokio::test]
async fn reference_statsd_receiver_aggregates_our_counter_gauge_timer_and_set() {
    // The real reference collector loads this small backend solely to make its
    // parsed aggregates machine-readable. No metric framing/parser lives here.
    let directory = tempfile::tempdir().unwrap();
    let backend = directory.path().join("observe.js");
    std::fs::write(&backend,r#"
exports.init = function(startupTime, config, events) {
  events.on('flush', function(timestamp, metrics) {
    if (metrics.counters['interop.counter'] === 6 && metrics.gauges['interop.gauge'] === 7 && metrics.timers['interop.timer'] && metrics.timers['interop.timer'].length === 1) {
      process.stdout.write('NETGET_RESULT ' + JSON.stringify({counter:metrics.counters['interop.counter'], gauge:metrics.gauges['interop.gauge'], timer:metrics.timers['interop.timer'], set:metrics.sets['interop.set'].values()}) + '\n');
    }
  });
  return true;
};
"#).unwrap();
    let bootstrap = r#"
// Compatibility only: Node 24+ removed util.log used by StatsD 0.9.0.
require('util').log ||= console.log;
const statsd = require.resolve('statsd/stats.js');
// StatsD treats configured port zero as 8125/8126. Force OS-assigned
// loopback ports in the harness; its packet parser and aggregation are untouched.
const net = require('net');
const listen = net.Server.prototype.listen;
net.Server.prototype.listen = function(_port, _host) { return listen.call(this, 0, '127.0.0.1'); };
const dgram = require('dgram');
const create = dgram.createSocket;
dgram.createSocket = function(...args) {
  const socket = create.apply(dgram,args);
  const bind = socket.bind;
  socket.bind = function(_port, _host) { return bind.call(socket, 0, '127.0.0.1'); };
  socket.once('listening', () => process.stdout.write('NETGET_PORT ' + socket.address().port + '\n'));
  return socket;
};
process.argv = [process.argv[0], statsd, process.argv[1]];
require(statsd);
"#;
    let config = directory.path().join("config.js");
    std::fs::write(&config,format!("{{ address: '127.0.0.1', port: 0, mgmt_port: 0, flushInterval: 100, backends: [{}], deleteCounters: false, deleteTimers: false, deleteSets: false }}",serde_json::to_string(&backend).unwrap())).unwrap();
    let mut child = Command::new("node")
        .args(["-e", bootstrap])
        .arg(&config)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("node and statsd@0.9.0 required");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("reference StatsD exited; install statsd@0.9.0 and set NODE_PATH");
            if let Some(port) = line.strip_prefix("NETGET_PORT ") {
                break port.parse::<u16>().unwrap();
            }
        }
    })
    .await
    .expect("reference StatsD bind");
    let (state, id) = start(
        format!("127.0.0.1:{port}"),
        json!({"type":"static","actions":[]}),
        "statsd",
    )
    .await;
    let records = json!([
        {"kind":"metric","name":"interop.counter","value":"3","metric_type":"c","sample_rate":0.5},
        {"kind":"metric","name":"interop.gauge","value":"5","metric_type":"g"},
        {"kind":"metric","name":"interop.gauge","value":"+2","metric_type":"g"},
        {"kind":"metric","name":"interop.timer","value":"12","metric_type":"ms"},
        {"kind":"metric","name":"interop.set","value":"alice","metric_type":"s"},
        {"kind":"metric","name":"interop.set","value":"alice","metric_type":"s"}
    ]);
    assert!(matches!(
        send(&state, id, records).await,
        ClientSendOutcome::Sent { .. }
    ));
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines
                .next_line()
                .await
                .unwrap()
                .expect("collector exited before flush");
            if let Some(result) = line.strip_prefix("NETGET_RESULT ") {
                break serde_json::from_str::<serde_json::Value>(result).unwrap();
            }
        }
    })
    .await
    .expect("reference collector did not parse and aggregate metrics");
    assert_eq!(
        result,
        json!({"counter":6,"gauge":7,"timer":[12],"set":["alice"]})
    );
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
