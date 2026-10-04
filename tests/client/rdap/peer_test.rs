//! NetGet's RDAP client against ICANN's rdap-srv 1.0.0 — an independent server, unchanged,
//! serving objects created by its own rdap-srv-data tool. The client's handler chains every
//! lookup class, a search, help, a 404 and a 307 referral; each answer's envelope is checked by
//! the client before the event is raised. Fails, never skips, when the peer is absent.
use crate::helpers::rdap::*;
use netget::state::AccessLogOwner;
use serde_json::json;
use std::time::Duration;

async fn data(dir: &std::path::Path, args: &[&str]) {
    let out = tokio::process::Command::new(icann_rdap_srv_data())
        .arg("--data-dir")
        .arg(dir)
        .args(args)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "rdap-srv-data {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn client_walks_icann_rdap_srv_through_every_answer_shape() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    data(
        d,
        &["nameserver", "--ldh", "ns1.example.com", "--handle", "NS-1"],
    )
    .await;
    data(
        d,
        &[
            "domain",
            "--ldh",
            "example.com",
            "--handle",
            "EX-1",
            "--status",
            "active",
            "--ns",
            "ns1.example.com",
        ],
    )
    .await;
    data(
        d,
        &[
            "autnum",
            "--start-autnum",
            "64496",
            "--end-autnum",
            "64496",
            "--handle",
            "AS64496",
        ],
    )
    .await;
    data(
        d,
        &["network", "--cidr", "192.0.2.0/24", "--handle", "NET-1"],
    )
    .await;
    data(
        d,
        &[
            "--redirect",
            "http://127.0.0.1:9/rdap/domain/moved.example",
            "domain",
            "--ldh",
            "moved.example",
        ],
    )
    .await;
    data(d, &["srv-help", "--notice", "ICANN test help"]).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut server = tokio::process::Command::new(icann_rdap_srv())
        .env("RDAP_SRV_LISTEN_ADDR", "127.0.0.1")
        .env("RDAP_SRV_LISTEN_PORT", port.to_string())
        .env("RDAP_SRV_STORAGE", "memory")
        .env("RDAP_SRV_DATA_DIR", d)
        .env("RDAP_SRV_DOMAIN_SEARCH_BY_NAME", "true")
        .env("RDAP_SRV_LOG", "warn")
        .kill_on_drop(true)
        .spawn()
        .expect("start rdap-srv");
    for _ in 0..200 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let state = state();
    let chain = concat!(
        "import json,sys\n",
        "e=json.load(sys.stdin)['event']\n",
        "steps=[('domain','example.com'),('nameserver','ns1.example.com'),('ip','192.0.2.7'),('autnum','64496'),('domains','exa*'),('help',None),('domain','nope.example'),('domain','moved.example')]\n",
        "done=[(s,v) for s,v in steps if s==e.get('query_type') and v==e.get('value')]\n",
        "i=0 if 'base_url' in e else steps.index(done[0])+1\n",
        "a=[] if i>=len(steps) else [dict({'type':'rdap_query','query_type':steps[i][0]}, **({'value':steps[i][1]} if steps[i][1] else {}))]\n",
        "print(json.dumps({'actions':a}))\n"
    );
    let handlers = vec![
        json!({"event_pattern":"rdap_ready","handler":{"type":"script","language":"python","code":chain}}),
        json!({"event_pattern":"rdap_response","handler":{"type":"script","language":"python","code":chain}}),
    ];
    let cid = client_in(
        &state,
        format!("127.0.0.1:{port}"),
        handlers,
        json!({"base_path": "/rdap"}),
    )
    .await;
    let rows = logs(
        &state,
        AccessLogOwner::Client(cid.as_u32()),
        "rdap_response",
        8,
    )
    .await;
    let r = |i: usize| &rows[i].request;
    assert_eq!(
        (r(0)["status"].as_u64(), r(0)["object"]["handle"].as_str()),
        (Some(200), Some("EX-1"))
    );
    assert_eq!(r(1)["object"]["objectClassName"], "nameserver");
    assert_eq!(
        r(2)["object"]["handle"],
        "NET-1",
        "the address resolves to its covering network"
    );
    assert_eq!(r(3)["object"]["handle"], "AS64496");
    assert_eq!(r(4)["object"]["domainSearchResults"][0]["handle"], "EX-1");
    assert!(r(5)["object"]["notices"]
        .to_string()
        .contains("ICANN test help"));
    assert_eq!(
        (r(6)["status"].as_u64(), r(6)["error"]["errorCode"].as_u64()),
        (Some(404), Some(404))
    );
    assert_eq!(r(7)["status"], 307);
    assert_eq!(
        r(7)["redirect"],
        "http://127.0.0.1:9/rdap/domain/moved.example"
    );
    state.remove_client(cid).await;
    let _ = server.kill().await;
}
