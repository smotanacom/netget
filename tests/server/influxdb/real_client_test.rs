use super::e2e_test::{logs, start};
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn official_python_client_writes_five_kinds_four_precisions_and_gzip() {
    let (state, id, addr) = start(None, Some(json!({"auth_token":"peer-secret"}))).await;
    let script = r#"
import importlib.metadata,sys
assert importlib.metadata.version('influxdb-client')=='1.50.0'
from influxdb_client import InfluxDBClient,Point,WritePrecision
from influxdb_client.client.write_api import SYNCHRONOUS
for gzip in [False,True]:
 with InfluxDBClient(url='http://127.0.0.1:'+sys.argv[1],token='peer-secret',org='org 名 &',enable_gzip=gzip,timeout=5000) as client:
  api=client.write_api(write_options=SYNCHRONOUS)
  for precision in [WritePrecision.NS,WritePrecision.US,WritePrecision.MS,WritePrecision.S]:
   p=Point.from_dict({'measurement':'温 度,','tags':{'host =,':'a,b =名'},'fields':{'float':1.25,'int':-42,'uint':18446744073709551615,'bool':True,'str =,':'say "hi" \\ literal\\n\t名'},'time':123},write_precision=precision,field_types={'uint':'uint'})
   api.write(bucket='bucket / &',record=p,write_precision=precision)
"#;
    let mut command = tokio::process::Command::new(
        std::env::var("NETGET_INFLUX_PYTHON")
            .expect("NETGET_INFLUX_PYTHON required; bootstrap pinned Influx peers"),
    );
    command
        .args(["-c", script, &addr.port().to_string()])
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "official Influx Python peer missing/failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let entries = logs(&state, id, 8).await;
    assert_eq!(entries.len(), 8);
    for e in &entries {
        let p = &e.request["points"][0];
        assert!(e.request["errors"].as_array().unwrap().is_empty());
        assert_eq!(e.request["org"], "org 名 &");
        assert_eq!(e.request["bucket"], "bucket / &");
        assert_eq!(p["measurement"], "温 度,");
        assert_eq!(p["tags"]["host =,"], "a,b =名");
        assert_eq!(p["fields"]["uint"]["value"], u64::MAX);
        assert_eq!(p["fields"]["float"]["type"], "float");
        assert_eq!(p["fields"]["int"]["value"], -42);
        assert_eq!(p["fields"]["bool"]["value"], true);
        assert_eq!(
            p["fields"]["str =,"]["value"],
            "say \"hi\" \\ literal\\n\t名"
        );
        let factor = match e.request["precision"].as_str().unwrap() {
            "ns" => 1,
            "us" => 1000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            _ => panic!("bad precision"),
        };
        assert_eq!(p["timestamp_ns"], 123i64 * factor);
        assert!(!e.request.to_string().contains("peer-secret"));
    }
    state.remove_server(id).await;
}
#[tokio::test]
async fn official_python_client_observes_json_rejection_and_retry_advice() {
    let(state,id,addr)=start(Some(vec![json!({"event_pattern":"influx_write","handler":{"type":"static","actions":[{"type":"reject_influx_points","status":429,"message":"Rate limited","retry_after_seconds":3}]}})]),None).await;
    let script = r#"
import importlib.metadata,json,sys
assert importlib.metadata.version('influxdb-client')=='1.50.0'
from influxdb_client import InfluxDBClient,Point
from influxdb_client.client.write_api import SYNCHRONOUS
from influxdb_client.rest import ApiException
with InfluxDBClient(url='http://127.0.0.1:'+sys.argv[1],org='example',timeout=5000) as c:
 try:c.write_api(write_options=SYNCHRONOUS).write(bucket='metrics',record=Point('m').field('f',1))
 except ApiException as e:
  assert e.status==429
  assert e.headers['Retry-After']=='3'
  body=json.loads(e.body)
  assert body['code']=='too many requests' and body['message']=='Rate limited'
 else:raise AssertionError('rejected write reported success')
"#;
    let mut command = tokio::process::Command::new(
        std::env::var("NETGET_INFLUX_PYTHON").expect("pinned official Influx Python required"),
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
        "official error-response peer: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    state.remove_server(id).await;
}
