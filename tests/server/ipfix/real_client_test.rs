use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn independent_python_exporter_sends_ipv4_ipv6_reduced_counters_strings_timestamps_and_options(
) {
    let (state, id, addr) = super::e2e_test::start(None, None).await;
    let python = std::env::var("NETGET_IPFIX_PYTHON")
        .expect("NETGET_IPFIX_PYTHON and pinned ipfix0.9.7 required; no missing-peer skip");
    let code = r#"from ipfix import ie,message,template
from ipaddress import ip_address
from datetime import datetime,timezone
import socket,sys
ie.use_iana_default()
m=message.MessageBuffer();m.begin_export(42);m.set_export_time(datetime(2024,3,9,16,0))
t=template.from_ielist(256,ie.spec_list(['sourceIPv4Address','destinationIPv4Address','sourceTransportPort','destinationTransportPort','protocolIdentifier','octetDeltaCount[4]','packetDeltaCount','flowStartSeconds','flowEndMilliseconds','interfaceName']))
m.add_template(t);m.export_new_set(256);m.export_namedict({'sourceIPv4Address':ip_address('192.0.2.1'),'destinationIPv4Address':ip_address('198.51.100.2'),'sourceTransportPort':1234,'destinationTransportPort':443,'protocolIdentifier':6,'octetDeltaCount':66051,'packetDeltaCount':9007199254740999,'flowStartSeconds':datetime(2024,3,9,16,0),'flowEndMilliseconds':datetime(2024,3,9,16,0,0,123000),'interfaceName':'eth-名\0'})
t6=template.from_ielist(257,ie.spec_list(['sourceIPv6Address','destinationIPv6Address','packetDeltaCount']))
m.add_template(t6);m.export_new_set(257);m.export_namedict({'sourceIPv6Address':ip_address('2001:db8::1'),'destinationIPv6Address':ip_address('2001:db8::2'),'packetDeltaCount':7})
o=template.from_ielist(258,ie.spec_list(['observationDomainId','samplingInterval','samplingAlgorithm']));o.scopecount=1
m.add_template(o);m.export_new_set(258);m.export_namedict({'observationDomainId':42,'samplingInterval':100,'samplingAlgorithm':1})
s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);s.sendto(m.to_bytes(),(sys.argv[1],int(sys.argv[2])));s.close();print('NETGET_IPFIX_PUBLIC_EXPORT 3')
"#;
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(python)
            .args(["-c", code, &addr.ip().to_string(), &addr.port().to_string()])
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("NETGET_IPFIX_PUBLIC_EXPORT 3"));
    let e = super::e2e_test::logs(&state, id, "ipfix_message", 1).await;
    let m = &e[0].request["message"];
    assert_eq!(m["record_count"], 3);
    assert_eq!(m["observation_domain_id"], 42);
    assert_eq!(
        m["data_sets"][0]["records"][0],
        json!([{"kind":"ipv4","value":"192.0.2.1"},{"kind":"ipv4","value":"198.51.100.2"},{"kind":"unsigned","value":1234},{"kind":"unsigned","value":443},{"kind":"unsigned","value":6},{"kind":"unsigned","value":66051},{"kind":"unsigned","value":9007199254740999u64},{"kind":"timestamp_seconds","value":1710000000},{"kind":"timestamp_milliseconds","value":1710000000123u64},{"kind":"string","value":"eth-名\0"}])
    );
    assert_eq!(
        m["data_sets"][1]["records"][0][0],
        json!({"kind":"ipv6","value":"2001:db8::1"})
    );
    assert_eq!(
        m["data_sets"][1]["records"][0][1],
        json!({"kind":"ipv6","value":"2001:db8::2"})
    );
    assert_eq!(m["data_sets"][2]["template"]["scope_count"], 1);
    assert_eq!(
        m["data_sets"][2]["records"][0],
        json!([{"kind":"unsigned","value":42},{"kind":"unsigned","value":100},{"kind":"unsigned","value":1}])
    );
    state.remove_server(id).await;
}
