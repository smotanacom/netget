//! NetGet's TR-069 ACS against **genieacs-sim** 0.9.0, the GenieACS project's CPE simulator: a
//! TR-098 device of a thousand parameters that informs, answers GetParameterValues,
//! SetParameterValues, GetParameterNames, AddObject and DeleteObject from its own data model,
//! and faults what it does not implement. A python policy is the model: on the device's
//! Inform it queues a read, a write, a read-back, a discovery, an add and a delete, and a
//! reboot the simulator refuses. Then raw HTTP for sessions, faults, bounds and a failed
//! handler. `install_peers.py` prints `NETGET_GENIEACS_SIM`; the test fails rather than skips
//! without it. No LLM calls.
use netget::cli::management::ServerForm;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

const ROOT: &str = "InternetGatewayDevice";

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
R='InternetGatewayDevice.'
a=[]
if t=='tr069_inform':
  if e['device_id']['serial_number']=='intruder':
    a=[{'type':'tr069_reject','message':'unknown device'}]
  else:
    a=[{'type':'tr069_get_parameter_values','names':[R+'DeviceInfo.SoftwareVersion',R+'ManagementServer.PeriodicInformInterval']},
       {'type':'tr069_set_parameter_values','parameters':{R+'ManagementServer.PeriodicInformInterval':3600},'parameter_key':'netget-1'},
       {'type':'tr069_get_parameter_values','names':[R+'ManagementServer.PeriodicInformInterval']},
       {'type':'tr069_get_parameter_names','path':R+'DeviceInfo.','next_level':True},
       {'type':'tr069_add_object','object':R+'LANDevice.1.WLANConfiguration.'},
       {'type':'tr069_reboot','command_key':'netget'}]
elif t=='tr069_response' and e['method']=='AddObject' and e['ok']:
  a=[{'type':'tr069_delete_object','object':R+'LANDevice.1.WLANConfiguration.%d.' % e['result']['instance_number']}]
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/tr069/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn start(handlers: Option<Vec<Value>>) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "tr069".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Manage home gateways".into()),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

async fn events(state: &AppState, id: ServerId, kind: &str) -> Vec<Value> {
    let mut v: Vec<Value> = state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == kind)
        .collect();
    v.sort_by_key(|e| e["id"].as_u64().unwrap_or(0));
    v.into_iter().map(|e| e["request"].clone()).collect()
}

fn values(r: &Value) -> serde_json::Map<String, Value> {
    r["result"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap().to_string(), p["value"].clone()))
        .collect()
}

#[tokio::test]
async fn genieacs_sim_is_managed_by_netget() {
    let (state, id, port) = start(policy()).await;
    let sim_log = tempfile::NamedTempFile::new().unwrap();
    let _sim = tokio::process::Command::new("node")
        .arg(env("NETGET_GENIEACS_SIM"))
        .arg(format!("http://127.0.0.1:{port}/"))
        .arg("000042")
        .stdout(sim_log.reopen().unwrap())
        .stderr(sim_log.reopen().unwrap())
        .kill_on_drop(true)
        .spawn()
        .expect("node");
    // Six RPCs and the delete the add leads to: seven responses.
    let responses = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let r = events(&state, id, "tr069_response").await;
            if r.len() >= 7 {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the session did not finish: {}",
            std::fs::read_to_string(sim_log.path()).unwrap_or_default()
        )
    });

    let inform = &events(&state, id, "tr069_inform").await[0];
    assert_eq!(inform["device_id"]["serial_number"], "000042", "{inform}");
    assert_eq!(
        inform["device_id"]["manufacturer"],
        "Huawei Technologies Co., Ltd."
    );
    assert_eq!(inform["events"][0]["code"], "2 PERIODIC");
    assert!(
        inform["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == format!("{ROOT}.DeviceInfo.SoftwareVersion")),
        "{inform}"
    );

    let methods: Vec<&str> = responses
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        // Queued RPCs go in order; the delete the add led to joins the end of the queue.
        [
            "GetParameterValues",
            "SetParameterValues",
            "GetParameterValues",
            "GetParameterNames",
            "AddObject",
            "Reboot",
            "DeleteObject"
        ],
        "{responses:?}"
    );
    let first = values(&responses[0]);
    assert_eq!(
        first[&format!("{ROOT}.DeviceInfo.SoftwareVersion")],
        "V100R001IRQC56B017"
    );
    assert_eq!(
        first[&format!("{ROOT}.ManagementServer.PeriodicInformInterval")],
        "10"
    );
    assert_eq!(responses[1]["result"]["status"], 0);
    // The simulator stored what NetGet set, type and all.
    let after = values(&responses[2]);
    assert_eq!(
        after[&format!("{ROOT}.ManagementServer.PeriodicInformInterval")],
        "3600"
    );
    assert_eq!(
        responses[2]["result"]["parameters"][0]["type"],
        "xsd:unsignedInt"
    );
    let names = responses[3]["result"]["parameters"].as_array().unwrap();
    let version = names
        .iter()
        .find(|n| n["name"] == format!("{ROOT}.DeviceInfo.SoftwareVersion"))
        .unwrap();
    assert_eq!(version["writable"], false);
    assert!(names.len() > 5, "{names:?}");
    assert_eq!(responses[4]["result"]["status"], 0);
    assert!(responses[4]["result"]["instance_number"].as_u64().unwrap() >= 1);
    // The simulator implements no Reboot: its fault reaches the model.
    assert_eq!(responses[5]["ok"], false);
    assert_eq!(responses[5]["fault"]["code"], 9000);
    assert_eq!(responses[5]["fault"]["message"], "Method not supported");
    assert_eq!(responses[6]["ok"], true, "{:?}", responses[6]);
    assert_eq!(responses[6]["result"]["status"], 0);
}

const INFORM: &str = r#"<?xml version="1.0"?><soap-env:Envelope xmlns:soap-env="http://schemas.xmlsoap.org/soap/envelope/" xmlns:cwmp="urn:dslforum-org:cwmp-1-0"><soap-env:Header><cwmp:ID>1</cwmp:ID></soap-env:Header><soap-env:Body><cwmp:Inform><DeviceId><Manufacturer>m</Manufacturer><OUI>000000</OUI><ProductClass>p</ProductClass><SerialNumber>SERIAL</SerialNumber></DeviceId><Event><EventStruct><EventCode>1 BOOT</EventCode><CommandKey></CommandKey></EventStruct></Event><MaxEnvelopes>1</MaxEnvelopes><CurrentTime>2026-01-01T00:00:00Z</CurrentTime><RetryCount>0</RetryCount><ParameterList></ParameterList></cwmp:Inform></soap-env:Body></soap-env:Envelope>"#;

async fn post(port: u16, body: String, cookie: Option<&str>) -> (u16, String, Option<String>) {
    let mut req = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/"))
        .body(body);
    if let Some(c) = cookie {
        req = req.header("Cookie", c);
    }
    let r = req.send().await.unwrap();
    let cookie = r
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::to_string);
    (r.status().as_u16(), r.text().await.unwrap(), cookie)
}

fn fault_code(xml: &str) -> u64 {
    netget::server::tr069::wire::parse(xml.as_bytes())
        .unwrap()
        .content["code"]
        .as_u64()
        .unwrap_or(0)
}

#[tokio::test]
async fn sessions_faults_and_bounds() {
    let (state, id, port) = start(policy()).await;
    // A refused device: fault 8001 with the model's reason, no session cookie.
    let (_, body, cookie) = post(port, INFORM.replace("SERIAL", "intruder"), None).await;
    assert_eq!(fault_code(&body), 8001, "{body}");
    assert!(
        body.contains("unknown device") && cookie.is_none(),
        "{body}"
    );
    // Anything but Inform outside a session.
    let (_, body, _) = post(
        port,
        INFORM.replace("cwmp:Inform>", "cwmp:GetRPCMethodsResponse>"),
        None,
    )
    .await;
    assert_eq!(fault_code(&body), 8003, "{body}");
    // A DOCTYPE (entity expansion) and nesting past the bound are refused unread.
    let doctype = format!("<!DOCTYPE x [<!ENTITY a \"aaaa\">]>{INFORM}");
    assert_eq!(fault_code(&post(port, doctype, None).await.1), 8003);
    let deep = format!(
        "<soap-env:Envelope xmlns:soap-env=\"http://schemas.xmlsoap.org/soap/envelope/\"><soap-env:Body>{}{}</soap-env:Body></soap-env:Envelope>",
        "<a>".repeat(netget::server::tr069::wire::MAX_DEPTH + 2),
        "</a>".repeat(netget::server::tr069::wire::MAX_DEPTH + 2)
    );
    assert_eq!(fault_code(&post(port, deep, None).await.1), 8003);
    let (status, _, _) = post(
        port,
        "x".repeat(netget::server::tr069::wire::MAX_ENVELOPE + 1),
        None,
    )
    .await;
    assert_eq!(status, 413);
    // A session: InformResponse with a cookie, then the queued RPCs one per post.
    let (_, body, cookie) = post(port, INFORM.to_string(), None).await;
    assert!(body.contains("InformResponse"), "{body}");
    let cookie = cookie.expect("a session cookie");
    let (_, body, _) = post(port, String::new(), Some(&cookie)).await;
    let rpc = netget::server::tr069::wire::parse(body.as_bytes()).unwrap();
    assert_eq!(rpc.method, "GetParameterValues", "{body}");
    // Answering with a fault moves the session on to the next RPC.
    let fault = netget::server::tr069::wire::envelope(
        rpc.id.as_deref().unwrap(),
        &netget::server::tr069::wire::fault(9005, "Invalid parameter name"),
    );
    let (_, body, _) = post(port, fault, Some(&cookie)).await;
    assert_eq!(
        netget::server::tr069::wire::parse(body.as_bytes())
            .unwrap()
            .method,
        "SetParameterValues",
        "{body}"
    );
    let r = events(&state, id, "tr069_response").await;
    assert_eq!(
        r[0]["fault"],
        json!({"code": 9005, "message": "Invalid parameter name"})
    );

    // No model: the Inform is answered with fault 8002 and a category, and no session opens.
    let (_s2, _id2, port2) = start(None).await;
    let (_, body, cookie) = post(port2, INFORM.to_string(), None).await;
    assert_eq!(fault_code(&body), 8002, "{body}");
    assert!(
        cookie.is_none() && !body.contains("127.0.0.1") && !body.contains("LLM"),
        "{body}"
    );
}
