//! NetGet's WS-Discovery target service against **python WSDiscovery** (an independent
//! implementation, run as a subprocess), failing rather than skipping without it. The python
//! interpreter is `NETGET_WSD_PYTHON` (default `python3`); it must import `wsdiscovery`
//! (`pip install WSDiscovery`).
//!
//! `POLICY` plays two cameras: A answers with its address, B without one, which makes the
//! python client send a multicast Resolve that only B's answer satisfies. A probe for any other
//! type is declined the WS-Discovery way: silence. No LLM calls.
use netget::cli::management::ServerForm;
use netget::server::wsdiscovery::wire::{self, Kind, QName, Version};
use netget::state::app_state::AppState;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;

pub const NVT: &str = "{http://www.onvif.org/ver10/network/wsdl}NetworkVideoTransmitter";
pub const A: &str = "urn:uuid:aaaaaaaa-0000-4000-8000-000000000001";
pub const B: &str = "urn:uuid:bbbbbbbb-0000-4000-8000-000000000002";

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
NVT='{http://www.onvif.org/ver10/network/wsdl}NetworkVideoTransmitter'
A={'endpoint_reference':'urn:uuid:aaaaaaaa-0000-4000-8000-000000000001','types':[NVT],'scopes':['onvif://www.onvif.org/name/CameraA'],'xaddrs':['http://192.0.2.10/onvif/device_service'],'metadata_version':3}
B={'endpoint_reference':'urn:uuid:bbbbbbbb-0000-4000-8000-000000000002','types':[NVT],'scopes':['onvif://www.onvif.org/name/CameraB']}
a=[]
if t=='wsd_probe' and (not e['types'] or NVT in e['types']):
  a=[{'type':'wsd_probe_match','matches':[A,B]}]
elif t=='wsd_resolve' and e['endpoint_reference']==B['endpoint_reference']:
  a=[dict(B,type='wsd_resolve_match',xaddrs=['http://192.0.2.20/onvif/device_service'])]
print(json.dumps({'actions':a}))"#;

pub async fn start(port: u16, join: bool) -> (AppState, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "wsdiscovery".into(),
        port: Some(port),
        host: Some("0.0.0.0".into()),
        instruction: Some("Be two cameras".into()),
        event_handlers: Some(vec![json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}})]),
        startup_params: Some(json!({"join_multicast": join})),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, port)
}

/// The python interpreter with `wsdiscovery`, or a failure saying how to get one.
pub fn python() -> String {
    let python = std::env::var("NETGET_WSD_PYTHON").unwrap_or_else(|_| "python3".into());
    let ok = std::process::Command::new(&python)
        .args(["-c", "import wsdiscovery"])
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "{python} cannot import wsdiscovery: pip install WSDiscovery, or point NETGET_WSD_PYTHON at a venv's python");
    python
}

const SEARCH: &str = r#"import json, sys
from wsdiscovery import WSDiscovery, QName
host, port, local = sys.argv[1], sys.argv[2], sys.argv[3]
w = WSDiscovery()
w.start()
ns, name = local[1:].split('}')
types = [QName(ns, name)]
found = w.searchServices(types=types, address=host or None, port=int(port) if port else None, timeout=3)
print(json.dumps(sorted([{'epr': s.getEPR(), 'types': ['{%s}%s' % (t.getNamespace(), t.getLocalname()) for t in s.getTypes()],
    'scopes': [x.getValue() for x in s.getScopes()], 'xaddrs': s.getXAddrs()} for s in found], key=lambda s: s['epr'])))
w.stop()
"#;

/// A directed probe through the library's own message codec over a plain socket.
/// `WSDiscovery.searchServices(address=..)` cannot be used: python-ws-discovery 2.1 sends a
/// directed probe from `_uniOutSocket`, which it never registers for reading, so every reply
/// to a directed probe is lost whatever the target does (checked against a stub responder).
/// The serializer and the parser are what matter here, and those are the library's.
const DIRECTED: &str = r#"import json, socket, sys
from wsdiscovery import QName
from wsdiscovery.actions.probe import constructProbe
from wsdiscovery.message import createSOAPMessage, parseSOAPMessage
host, port, local = sys.argv[1], int(sys.argv[2]), sys.argv[3]
ns, name = local[1:].split('}')
env = constructProbe([QName(ns, name)], [])
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(3)
s.sendto(createSOAPMessage(env).encode(), (host, port))
found = []
try:
    while True:
        data, _ = s.recvfrom(65536)
        reply = parseSOAPMessage(data, host)
        if reply.getRelatesTo() != env.getMessageId():
            continue
        for m in reply.getProbeResolveMatches():
            found.append({'epr': m.getEPR(), 'types': ['{%s}%s' % (t.getNamespace(), t.getLocalname()) for t in m.getTypes()],
                'scopes': [x.getValue() for x in m.getScopes()], 'xaddrs': m.getXAddrs(), 'metadata_version': m.getMetadataVersion()})
except socket.timeout:
    pass
print(json.dumps(sorted(found, key=lambda s: s['epr'])))
"#;

/// Run one python search: directed (library codec, plain socket) when `host` is given,
/// the library's multicast search otherwise.
pub async fn search(host: &str, port: Option<u16>, local: &str) -> Vec<Value> {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("search.py");
    std::fs::write(&script, if host.is_empty() { SEARCH } else { DIRECTED }).unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(python())
            .args([
                "-I",
                script.to_str().unwrap(),
                host,
                &port.map(|p| p.to_string()).unwrap_or_default(),
                local,
            ])
            .output(),
    )
    .await
    .expect("the python search did not finish")
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(text.lines().last().unwrap_or("[]")).unwrap_or_else(|_| panic!("{text}"))
}

#[tokio::test]
async fn python_wsdiscovery_directed_probe() {
    let (_state, port) = start(0, false).await;
    let found = search("127.0.0.1", Some(port), NVT).await;
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0]["epr"], A);
    assert_eq!(found[0]["types"], json!([NVT]));
    assert_eq!(
        found[0]["scopes"],
        json!(["onvif://www.onvif.org/name/CameraA"])
    );
    assert_eq!(
        found[0]["xaddrs"],
        json!(["http://192.0.2.10/onvif/device_service"])
    );
    assert_eq!(found[0]["metadata_version"], "3");
    assert_eq!(found[1]["epr"], B);
    // A type neither camera has: silence, so python finds nothing.
    let none = search(
        "127.0.0.1",
        Some(port),
        "{http://schemas.xmlsoap.org/ws/2006/02/devprof}Printer",
    )
    .await;
    assert!(none.is_empty(), "{none:?}");
}

/// Port 3702 is shared (SO_REUSEADDR), but one multicast test at a time keeps the answers
/// countable.
static MULTICAST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn python_wsdiscovery_multicast_probe_and_resolve() {
    let _one = MULTICAST.lock().await;
    let (_state, _port) = start(wire::PORT, true).await;
    let found = search("", None, NVT).await;
    let b = found
        .iter()
        .find(|s| s["epr"] == B)
        .unwrap_or_else(|| panic!("camera B not found: {found:?}"));
    // B answered the probe without an address; python resolved it on the group, and the
    // ResolveMatch is where this address came from.
    assert_eq!(
        b["xaddrs"],
        json!(["http://192.0.2.20/onvif/device_service"]),
        "{found:?}"
    );
    assert!(found.iter().any(|s| s["epr"] == A), "{found:?}");
}

#[tokio::test]
async fn version_2009_is_answered_in_2009() {
    let (_state, port) = start(0, false).await;
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let id = wire::new_message_id();
    let probe = wire::probe(
        Version::V2009,
        &id,
        &[QName::parse(NVT).unwrap()],
        &[],
        None,
    );
    socket
        .send_to(probe.as_bytes(), ("127.0.0.1", port))
        .await
        .unwrap();
    let mut buf = vec![0u8; 65_536];
    let (n, _) = tokio::time::timeout(Duration::from_secs(20), socket.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let reply = wire::parse(&buf[..n]).unwrap();
    assert_eq!(
        (reply.version, reply.kind),
        (Version::V2009, Kind::ProbeMatches)
    );
    assert_eq!(reply.relates_to.as_deref(), Some(id.as_str()));
    assert_eq!(reply.targets.len(), 2);
    assert!(String::from_utf8_lossy(&buf[..n])
        .contains("http://docs.oasis-open.org/ws-dd/ns/discovery/2009/01"));

    // Nesting past the bound is dropped, and the server goes on answering.
    let mut deep = String::from("<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body>");
    deep.push_str(&"<x>".repeat(wire::MAX_DEPTH));
    deep.push_str(&"</x>".repeat(wire::MAX_DEPTH));
    deep.push_str("</s:Body></s:Envelope>");
    socket
        .send_to(deep.as_bytes(), ("127.0.0.1", port))
        .await
        .unwrap();
    let id = wire::new_message_id();
    socket
        .send_to(
            wire::probe(Version::V2005, &id, &[], &[], None).as_bytes(),
            ("127.0.0.1", port),
        )
        .await
        .unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(20), socket.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let reply = wire::parse(&buf[..n]).unwrap();
    assert_eq!(
        reply.relates_to.as_deref(),
        Some(id.as_str()),
        "the deep datagram was answered"
    );
}

/// The codec's own refusals, without a server.
#[test]
fn parser_bounds() {
    let deep = format!(
        "<a>{}{}</a>",
        "<x>".repeat(wire::MAX_DEPTH),
        "</x>".repeat(wire::MAX_DEPTH)
    );
    assert!(wire::parse(deep.as_bytes())
        .unwrap_err()
        .to_string()
        .contains("nest more than"));
    let types: Vec<QName> = (0..wire::MAX_ITEMS + 1)
        .map(|i| QName {
            ns: "urn:x".into(),
            local: format!("T{i}"),
        })
        .collect();
    let probe = wire::probe(Version::V2005, "urn:uuid:1", &types, &[], None);
    assert!(
        wire::parse(probe.as_bytes()).is_err(),
        "a list past MAX_ITEMS parsed"
    );
    let fine = wire::probe(
        Version::V2005,
        "urn:uuid:1",
        &types[..wire::MAX_ITEMS],
        &[],
        None,
    );
    assert_eq!(
        wire::parse(fine.as_bytes()).unwrap().types.len(),
        wire::MAX_ITEMS
    );
}
