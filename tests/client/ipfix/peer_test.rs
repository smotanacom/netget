use super::e2e_test::{send, start};
use base64::{engine::general_purpose::STANDARD, Engine};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

fn batch() -> Value {
    json!({"observation_domain_id":42,"export_time":1710000000,"templates":[
        {"id":256,"fields":[{"element":"source_ipv4_address"},{"element":"destination_ipv4_address"},{"element":"source_transport_port"},{"element":"destination_transport_port"},{"element":"protocol_identifier"},{"element":"octet_delta_count","length":4},{"element":"packet_delta_count"},{"element":"flow_start_seconds"},{"element":"flow_end_milliseconds"},{"element":"interface_name"}]},
        {"id":257,"fields":[{"element":"source_ipv6_address"},{"element":"destination_ipv6_address"},{"element":"packet_delta_count"}]},
        {"id":258,"scope_count":1,"fields":[{"element":"observation_domain_id"},{"element":"sampling_interval"},{"element":"sampling_algorithm"}]}
    ],"data_sets":[
        {"template_id":256,"records":[[{"kind":"ipv4","value":"192.0.2.1"},{"kind":"ipv4","value":"198.51.100.2"},{"kind":"unsigned","value":1234},{"kind":"unsigned","value":443},{"kind":"unsigned","value":6},{"kind":"unsigned","value":66051},{"kind":"unsigned","value":9007199254740999u64},{"kind":"timestamp_seconds","value":1710000000},{"kind":"timestamp_milliseconds","value":1710000000123u64},{"kind":"string","value":"eth-名\0"}]]},
        {"template_id":257,"records":[[{"kind":"ipv6","value":"2001:db8::1"},{"kind":"ipv6","value":"2001:db8::2"},{"kind":"unsigned","value":7}]]},
        {"template_id":258,"records":[[{"kind":"unsigned","value":42},{"kind":"unsigned","value":100},{"kind":"unsigned","value":1}]]}
    ]})
}

#[tokio::test]
async fn independent_python_public_decoder_receives_all_value_classes_and_options_sequences() {
    let python = std::env::var("NETGET_IPFIX_PYTHON")
        .expect("NETGET_IPFIX_PYTHON and pinned ipfix0.9.7 required; no missing-peer skip");
    let code = r#"from ipfix import ie,message
from datetime import datetime,timezone
import socket,struct,json
ie.use_iana_default(); m=message.MessageBuffer()
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.bind(('127.0.0.1',0));s.settimeout(10)
print(s.getsockname()[1],flush=True)
for seq in (0,3):
 b=s.recv(8193);assert struct.unpack('!HHIII',b[:16])==(10,len(b),1710000000,seq,42)
 m.from_bytes(b);r=list(m.namedict_iterator());assert len(r)==3
 a=r[0];assert str(a['sourceIPv4Address'])=='192.0.2.1' and str(a['destinationIPv4Address'])=='198.51.100.2'
 assert (a['sourceTransportPort'],a['destinationTransportPort'],a['protocolIdentifier'],a['octetDeltaCount'],a['packetDeltaCount'])==(1234,443,6,66051,9007199254740999)
 assert a['flowStartSeconds']==datetime(2024,3,9,16) and a['flowEndMilliseconds']==datetime(2024,3,9,16,0,0,123000)
 assert a['interfaceName']=='eth-名\0'
 assert str(r[1]['sourceIPv6Address'])=='2001:db8::1' and str(r[1]['destinationIPv6Address'])=='2001:db8::2' and r[1]['packetDeltaCount']==7
 assert r[2]=={'observationDomainId':42,'samplingInterval':100,'samplingAlgorithm':1}
 assert m.templates[(42,258)].scopecount==1
 print(json.dumps({'sequence':seq,'records':3,'scope':1,'independent_decoder':'ipfix0.9.7'}),flush=True)
s.close()
"#;
    let mut child = tokio::process::Command::new(python)
        .args(["-c", code])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let port = tokio::time::timeout(Duration::from_secs(10), output.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("peer must announce its bound socket")
        .parse::<u16>()
        .unwrap();
    let (state, id) = start(format!("127.0.0.1:{port}"), None, None).await;
    for seq in [0, 3] {
        assert!(matches!(
            send(&state, id, batch()).await,
            ClientSendOutcome::Executed { .. }
        ));
        let line = tokio::time::timeout(Duration::from_secs(10), output.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("independent peer must confirm decoded records");
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"sequence":seq,"records":3,"scope":1,"independent_decoder":"ipfix0.9.7"})
        );
    }
    state.remove_client(id).await;
    let result = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn official_goflow2_227_service_decodes_native_templates_values_and_options() {
    let binary = std::env::var("NETGET_IPFIX_COLLECTOR")
        .expect("NETGET_IPFIX_COLLECTOR required; run install_peers.py, no missing-service skip");
    let version = tokio::process::Command::new(&binary)
        .arg("-v")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("2.2.7"));
    let root = tempfile::Builder::new()
        .prefix("netget-ipfix-goflow-")
        .tempdir()
        .unwrap();
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let records = root.path().join("records.json");
    let log_path = root.path().join("daemon.log");
    let stdout = std::fs::File::create(&records).unwrap();
    let stderr = std::fs::File::create(&log_path).unwrap();
    let mut child = tokio::process::Command::new(binary)
        .args([
            "-listen",
            &format!("netflow://127.0.0.1:{port}"),
            "-addr",
            "127.0.0.1:0",
            "-produce",
            "raw",
            "-format",
            "json",
            "-transport",
            "file",
        ])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // Only fixture readiness is retried. Native production exports are each sent once.
    let header = [0, 10, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 231];
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "collector exit: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            probe
                .send_to(&header, format!("127.0.0.1:{port}"))
                .await
                .unwrap();
            if read_records(&records)
                .iter()
                .any(|r| r["type"] == "ipfix" && r["message"]["observation-domain-id"] == 999)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "official collector readiness: {}",
            std::fs::read_to_string(&log_path).unwrap()
        )
    });
    let (state, id) = start(format!("127.0.0.1:{port}"), None, None).await;
    for _ in 0..2 {
        assert!(matches!(
            send(&state, id, batch()).await,
            ClientSendOutcome::Executed { .. }
        ));
    }
    let messages = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let messages = read_records(&records)
                .into_iter()
                .filter(|r| r["message"]["observation-domain-id"] == 42)
                .collect::<Vec<_>>();
            if messages.len() >= 2 {
                break messages;
            }
            assert!(child.try_wait().unwrap().is_none(), "collector exited");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let expected_values: Vec<Vec<Vec<u8>>> = vec![
        vec![
            vec![192, 0, 2, 1],
            vec![198, 51, 100, 2],
            1234u16.to_be_bytes().to_vec(),
            443u16.to_be_bytes().to_vec(),
            vec![6],
            66051u32.to_be_bytes().to_vec(),
            9007199254740999u64.to_be_bytes().to_vec(),
            1710000000u32.to_be_bytes().to_vec(),
            1710000000123u64.to_be_bytes().to_vec(),
            "eth-名\0".as_bytes().to_vec(),
        ],
        vec![
            "2001:db8::1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets()
                .to_vec(),
            "2001:db8::2"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets()
                .to_vec(),
            7u64.to_be_bytes().to_vec(),
        ],
        vec![
            42u32.to_be_bytes().to_vec(),
            100u32.to_be_bytes().to_vec(),
            vec![1],
        ],
    ];
    assert_eq!(messages.len(), 2, "exactly two domain-42 exports");
    // GoFlow2 decodes packets on parallel workers; file order is not wire order.
    for seq in [0, 3] {
        let matching = messages
            .iter()
            .filter(|r| r["message"]["sequence-number"] == seq)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            1,
            "one domain-42 export with sequence {seq}"
        );
        let r = matching[0];
        let m = &r["message"];
        assert_eq!(r["type"], "ipfix");
        assert_eq!(m["observation-domain-id"], 42);
        assert_eq!(m["version"], 10);
        assert_eq!(m["export-time"], 1710000000);
        assert_eq!(m["sequence-number"], seq);
        let sets = m["flow-sets"].as_array().unwrap();
        assert!(sets.iter().any(|s| s["id"] == 2
            && s["records"][0]["template-id"] == 256
            && s["records"][0]["field-count"] == 10));
        let options = sets
            .iter()
            .find(|s| s["id"] == 3)
            .expect("official decoder must expose options template");
        assert_eq!(options["records"][0]["template-id"], 258);
        assert_eq!(options["records"][0]["scope-field-count"], 1);
        for (id, expected) in [256, 257, 258].into_iter().zip(&expected_values) {
            let set = sets.iter().find(|s| s["id"] == id).unwrap();
            let record = &set["records"][0];
            let values = if id == 258 {
                let scope = record["scope-values"]
                    .as_array()
                    .expect("official options scope values");
                let options = record["option-values"]
                    .as_array()
                    .expect("official options values");
                assert_eq!(scope.len(), 1);
                assert_eq!(options.len(), 2);
                scope.iter().chain(options.iter()).collect::<Vec<_>>()
            } else {
                record["values"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .collect::<Vec<_>>()
            };
            let decoded = values
                .iter()
                .map(|v| STANDARD.decode(v["value"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                &decoded, expected,
                "official service field values for template{id}"
            );
        }
    }
    state.remove_client(id).await;
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
