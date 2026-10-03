use crate::helpers::tacacs::{logs, policies, server};
use netget::state::AccessLogOwner;
use serde_json::Value;
use std::time::Duration;
#[tokio::test]
async fn unmodified_go_sdk_client_exercises_ascii_pap_authorization_and_accounting() {
    let (state, id, addr, _) = server(Some(policies()), None).await;
    let binary = std::env::var("NETGET_TACACS_PEER").expect("pinned SDK client required; no skip");
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(binary)
            .args(["-addr", &addr.to_string()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str::<Value>(s).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["status"], 1);
    assert_eq!(rows[1]["kind"], 2);
    assert_eq!(rows[2]["Status"], 1);
    assert_eq!(rows[3]["Status"], 1);
    let auth = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "tacacs_authentication",
        2,
    )
    .await;
    assert_eq!(auth[0].request["request"]["username"], "alice");
    assert_eq!(auth[0].request["request"]["password"], "correct");
    assert_eq!(auth[1].request["request"]["method"], "pap");
    let a = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "tacacs_authorization",
        1,
    )
    .await;
    assert_eq!(a[0].request["request"]["arguments"][2]["value"], "version");
    let a = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "tacacs_accounting_recorded",
        1,
    )
    .await;
    assert_eq!(a[0].request["request"]["record_type"], "start");
    assert_eq!(a[0].request["request"]["arguments"][0]["value"], "42");
    assert_eq!(a[0].response[0]["durable_storage"], false);
    state.remove_server(id).await;
}
#[tokio::test]
async fn unmodified_python_client_exercises_negative_credentials_and_all_record_flags() {
    let (state, id, addr, _) = server(Some(policies()), None).await;
    let python = std::env::var("NETGET_TACACS_PYTHON")
        .expect("pinned isolated Python client required; no skip");
    let code = r#"import sys,json
from tacacs_plus.client import TACACSClient
import tacacs_plus.flags as f
h,p=sys.argv[1].split(':')
def client():return TACACSClient(h,int(p),'test-secret',timeout=5,session_id=0x01020304)
for kind in [f.TAC_PLUS_AUTHEN_TYPE_ASCII,f.TAC_PLUS_AUTHEN_TYPE_PAP]:
 for password in ['correct','wrong']:
  r=client().authenticate('alice',password,authen_type=kind,port='tty1',rem_addr='192.0.2.10')
  assert r.status==(1 if password=='correct' else 2)
  print(json.dumps({'method':kind,'accepted':r.status==1}))
r=client().authorize('alice',arguments=[b'service=shell',b'cmd=show',b'cmd-arg=version'])
assert r.status==1 and r.arguments==[b'priv-lvl=15',b'audit*enabled']
for flag in [2,4,8,10]:
 r=client().account('alice',flag,arguments=[b'task_id=42',b'start_time=1700000000'])
 assert r.status==1
 print(json.dumps({'record_type':flag,'status':r.status}))
"#;
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(python)
            .args(["-c", code, &addr.to_string()])
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
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 8);
    let rows = logs(
        &state,
        AccessLogOwner::Server(id.as_u32()),
        "tacacs_accounting_recorded",
        4,
    )
    .await;
    assert_eq!(
        rows.iter()
            .map(|e| e.request["request"]["record_type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["start", "stop", "watchdog", "update"]
    );
    for row in state.list_access_logs(None).await {
        assert!(!row.request.to_string().contains("test-secret"));
    }
    state.remove_server(id).await;
}
