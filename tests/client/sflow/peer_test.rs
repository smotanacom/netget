use super::e2e_test::{batch, receive, send, start};
use crate::helpers::sflow::{peer, peer_golden, Collector};
use base64::{engine::general_purpose::STANDARD, Engine};
use netget::state::client_handles::ClientSendOutcome;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;

#[tokio::test]
async fn independent_public_decoder_receives_native_ipv6_header_switch_and_normative_vlan_counters()
{
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(socket.local_addr().unwrap().to_string(), None).await;
    let mut b = batch();
    b["agent_address"] = json!("2001:db8::1");
    b["samples"].as_array_mut().unwrap().truncate(2);
    b["samples"][0]["records"] = json!([
        {"kind":"synthesized_header","frame_length":100,"stripped":4,"packet":{"source_ip":"2001:db8::2","destination_ip":"2001:db8::3","packet_length":96,"protocol":6,"source_port":1234,"destination_port":443,"tcp_flags":258,"traffic_class":176}},
        {"kind":"extended_switch","source_vlan":42,"source_priority":3,"destination_vlan":43,"destination_priority":4}
    ]);
    let root = tempfile::Builder::new()
        .prefix("netget-sflow-independent-decode-")
        .tempdir()
        .unwrap();
    let path = root.path().join("wire");
    for seq in [0, 1] {
        assert!(matches!(
            send(&state, id, b.clone()).await,
            ClientSendOutcome::Executed { .. }
        ));
        let (wire, _) = receive(&socket).await;
        std::fs::write(&path, &wire).unwrap();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new(peer())
                .args(["decode", path.to_str().unwrap()])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let decoded: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decoded["version"], 5);
        assert_eq!(decoded["ipVersion"], 2);
        assert_eq!(decoded["ipAddress"], "2001:db8::1");
        assert_eq!(decoded["sequenceNumber"], seq);
        assert_eq!(decoded["numSamples"], 2);
        assert_eq!(decoded["samples"][0]["SourceIdType"], 2);
        assert_eq!(decoded["samples"][0]["SourceIdIndexVal"], 3);
        let header = &decoded["samples"][0]["Records"][0];
        assert_eq!(header["Protocol"], 12);
        assert_eq!(header["HeaderSize"], 60);
        assert_eq!(header["FrameLength"], 100);
        assert_eq!(header["Stripped"], 4);
        let actual = STANDARD.decode(header["Header"].as_str().unwrap()).unwrap();
        let expected="6b00000000380640 20010db8000000000000000000000002 20010db8000000000000000000000003 04d201bb00000000000000005102000000000000".split_whitespace().collect::<String>();
        let literal = (0..expected.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&expected[at..at + 2], 16).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, literal);
        assert_eq!(
            decoded["samples"][0]["Records"][1],
            json!({"SourceVlan":42,"SourcePriority":3,"DestinationVlan":43,"DestinationPriority":4})
        );
        assert_eq!(
            decoded["samples"][1]["Records"][0],
            json!({"ID":42,"Octets":9007199254740999u64,"UnicastPackets":11,"MulticastPackets":12,"BroadcastPackets":13,"Discards":14})
        );
    }
    state.remove_client(id).await;
}

#[tokio::test]
async fn official_service_receives_native_all_four_sample_carriers_and_literal_vlan_bytes() {
    let mut service = Collector::start().await;
    let (state, id) = start(format!("127.0.0.1:{}", service.port), None).await;
    for _ in 0..2 {
        assert!(matches!(
            send(&state, id, batch()).await,
            ClientSendOutcome::Executed { .. }
        ));
    }
    let messages = service.messages(2).await;
    assert_eq!(messages.len(), 2);
    // GoFlow2 decodes on concurrent workers; match each exact datagram sequence
    // once without assuming its output preserves the UDP arrival order.
    for seq in [0, 1] {
        let rows = messages
            .iter()
            .filter(|row| row["message"]["sequence-number"] == seq)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "missing or duplicate native sequence {seq}");
        let row = rows[0];
        assert_eq!(row["type"], "sflow");
        let m = &row["message"];
        assert_eq!(m["version"], 5);
        assert_eq!(m["sequence-number"], seq);
        assert_eq!(m["uptime"], 123456);
        assert_eq!(m["samples-count"], 4);
        for (index, format, sample_seq) in [(0, 1, 5), (1, 2, 17), (2, 3, 6), (3, 4, 18)] {
            let s = &m["samples"][index];
            assert_eq!(s["header"]["format"], format);
            assert_eq!(s["header"]["sample-sequence-number"], sample_seq);
            assert_eq!(s["header"]["source-id-type"], 2);
            assert_eq!(s["header"]["source-id-value"], 3);
        }
        assert_eq!(m["samples"][0]["input"], 0x40000009u32);
        assert_eq!(m["samples"][0]["output"], 0x80000003u32);
        assert_eq!(m["samples"][2]["input-if-format"], 1);
        assert_eq!(m["samples"][2]["input-if-value"], 9);
        assert_eq!(m["samples"][2]["output-if-format"], 2);
        assert_eq!(m["samples"][2]["output-if-value"], 3);
        for i in [0, 2] {
            assert_eq!(
                m["samples"][i]["records"][0]["data"],
                json!({"length":64,"protocol":17,"src-ip":"192.0.2.2","dst-ip":"198.51.100.2","src-port":53,"dst-port":123,"tcp-flags":0,"tos":16})
            );
        }
        // GoFlow2 2.2.7 reports format5 as opaque, so assert the literal 28 wire
        // bytes. The independent Cistern decoder above asserts typed VLAN values.
        for i in [1, 3] {
            let record = &m["samples"][i]["records"][0];
            assert_eq!(record["header"]["data-format"], 5);
            assert_eq!(record["header"]["length"], 28);
            assert_eq!(
                STANDARD
                    .decode(record["data"]["data"].as_str().unwrap())
                    .unwrap(),
                vec![
                    0, 0, 0, 42, 0, 32, 0, 0, 0, 0, 0, 7, 0, 0, 0, 11, 0, 0, 0, 12, 0, 0, 0, 13, 0,
                    0, 0, 14
                ]
            );
        }
    }
    state.remove_client(id).await;
    service.stop().await;
}

#[tokio::test]
async fn independent_decoder_receives_native_all_interface_and_ethernet_counter_fields() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (state, id) = start(socket.local_addr().unwrap().to_string(), None).await;
    let mut b = batch();
    b["samples"] = json!([{"kind":"counters","sequence_number":17,"source":{"class":2,"index":3},"records":[
        {"kind":"interface","if_index":3,"if_type":6,"if_speed":1000000000,"if_direction":1,"if_status":3,
            "in_octets":9007199254740999u64,"in_unicast_packets":11,"in_multicast_packets":12,"in_broadcast_packets":13,
            "in_discards":14,"in_errors":15,"in_unknown_protocols":16,"out_octets":u64::MAX,"out_unicast_packets":21,
            "out_multicast_packets":22,"out_broadcast_packets":23,"out_discards":24,"out_errors":25,"promiscuous_mode":2},
        {"kind":"ethernet","alignment_errors":1,"fcs_errors":2,"single_collision_frames":3,"multiple_collision_frames":4,
            "sqe_test_errors":5,"deferred_transmissions":6,"late_collisions":7,"excessive_collisions":8,"internal_mac_transmit_errors":9,
            "carrier_sense_errors":10,"frame_too_longs":11,"internal_mac_receive_errors":12,"symbol_errors":13}
    ]}]);
    assert!(matches!(
        send(&state, id, b).await,
        ClientSendOutcome::Executed { .. }
    ));
    let (wire, _) = receive(&socket).await;
    // The Cistern emitter's counter sample follows its 116-byte flow sample.
    // Compare the whole native counter sample to the separately literal-checked
    // public emitter wire, then inspect every field through its public decoder.
    assert_eq!(&wire[28..], &peer_golden()[144..]);
    let root = tempfile::Builder::new()
        .prefix("netget-sflow-counter-decode-")
        .tempdir()
        .unwrap();
    let path = root.path().join("wire");
    std::fs::write(&path, wire).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(peer())
            .args(["decode", path.to_str().unwrap()])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decoded: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        decoded["samples"][0]["Records"][0],
        json!({"Index":3,"Type":6,"Speed":1000000000,"Direction":1,"Status":3,
        "InOctets":9007199254740999u64,"InUnicastPackets":11,"InMulticastPackets":12,"InBroadcastPackets":13,
        "InDiscards":14,"InErrors":15,"InUnknownProtocols":16,"OutOctets":u64::MAX,"OutUnicastPackets":21,
        "OutMulticastPackets":22,"OutBroadcastPackets":23,"OutDiscards":24,"OutErrors":25,"PromiscuousMode":2})
    );
    assert_eq!(
        decoded["samples"][0]["Records"][1],
        json!({"AlignmentErrors":1,"FCSErrors":2,"SingleCollisionFrames":3,
        "MultipleCollisionFrames":4,"SQETestErrors":5,"DeferredTransmissions":6,"LateCollisions":7,"ExcessiveCollisions":8,
        "InternalMACTransmitErrors":9,"CarrierSenseErrors":10,"FrameTooLongs":11,"InternalMACReceiveErrors":12,"SymbolErrors":13})
    );
    state.remove_client(id).await;
}
