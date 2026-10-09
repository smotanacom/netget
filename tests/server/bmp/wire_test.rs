//! The BMP codec and collector from the wire: per-peer headers and distinguishers round-trip;
//! NetGet's exporter reports every message type to NetGet's collector with its values intact;
//! the collector refuses a session that does not start with Initiation, a version other than 3,
//! an oversized or truncated message, and a router the handler rejects.
use crate::helpers::bmp::*;
use netget::server::bmp::codec::{self, PeerHeader};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[test]
fn headers_and_distinguishers_round_trip() {
    for (address, flags) in [
        (
            "192.0.2.9".parse::<IpAddr>().unwrap(),
            codec::FLAG_POST_POLICY,
        ),
        (
            "2001:db8::9".parse().unwrap(),
            codec::FLAG_V6 | codec::FLAG_LEGACY_AS,
        ),
    ] {
        let h = PeerHeader {
            peer_type: 1,
            flags,
            distinguisher: codec::parse_distinguisher("65000:42").unwrap(),
            address,
            asn: 4_200_000_000,
            bgp_id: Ipv4Addr::new(10, 1, 2, 3),
            seconds: 1_790_000_000,
            micros: 250_000,
        };
        let mut b = Vec::new();
        h.write(&mut b);
        assert_eq!(b.len(), codec::PEER_HEADER_LEN);
        assert_eq!(PeerHeader::read(&b).unwrap(), h);
        let j = h.to_json();
        assert_eq!(j["distinguisher"], "65000:42");
        assert_eq!(j["four_octet_as"], flags & codec::FLAG_LEGACY_AS == 0);
    }
    for rd in ["192.0.2.1:7", "4200000000:9", "1:1"] {
        assert_eq!(
            codec::distinguisher(&codec::parse_distinguisher(rd).unwrap()),
            rd
        );
    }
    for t in 0..=17 {
        assert_eq!(codec::stat_code(&codec::stat_name(t)), Some(t));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_exporter_to_netget_collector() {
    let state = state();
    let (sid, collector) = server_in(&state, policy()).await;
    let cid = client_in(
        &state,
        collector.to_string(),
        json!({"sys_name": "edge-3", "sys_descr": "lab"}),
    )
    .await
    .unwrap();
    let send = |a: serde_json::Value| state.send_to_client(cid, a, Duration::from_secs(20));
    for a in [
        json!({"type": "bmp_peer_up", "peer": {"address": "192.0.2.2", "asn": 4200000002u64, "bgp_id": "192.0.2.2", "type": "rd_instance", "distinguisher": "65000:5", "post_policy": true}, "local_address": "192.0.2.1", "local_asn": 65001, "local_bgp_id": "192.0.2.1", "remote_port": 40179, "hold_time": 30}),
        json!({"type": "bmp_route_monitoring", "peer_address": "192.0.2.2", "announce": ["203.0.113.0/24"], "next_hop": "192.0.2.2", "as_path": [4200000002u64, 65010], "origin": "EGP", "med": 20, "local_pref": 150, "communities": ["65002:100", "0:7"]}),
        json!({"type": "bmp_route_monitoring", "peer_address": "192.0.2.2", "withdraw": ["203.0.113.0/24"]}),
        json!({"type": "bmp_statistics", "peer_address": "192.0.2.2", "counters": [{"type": "rejected_prefixes", "value": 3}, {"type": "loc_rib_routes", "value": 5000000000u64}, {"type": "adj_rib_in_routes_per_afi_safi", "afi": 1, "safi": 1, "value": 9}]}),
        json!({"type": "bmp_peer_down", "peer_address": "192.0.2.2", "reason": "local_notification", "notification": {"code": 6, "subcode": 2}}),
    ] {
        let r = send(a.clone()).await.unwrap();
        assert!(matches!(r, ClientSendOutcome::Sent { .. }), "{a}: {r:?}");
    }
    // The peer is gone, and a malformed action is refused before the wire.
    for a in [
        json!({"type": "bmp_statistics", "peer_address": "192.0.2.2", "counters": [{"type": "rejected_prefixes", "value": 1}]}),
        json!({"type": "bmp_statistics", "peer_address": "192.0.2.2", "counters": [{"type": "no_such_counter", "value": 1}]}),
    ] {
        let r = send(a.clone()).await.unwrap();
        assert!(
            matches!(r, ClientSendOutcome::Rejected { .. }),
            "{a}: {r:?}"
        );
    }
    assert!(matches!(
        send(json!({"type": "bmp_termination", "reason": "out_of_resources", "message": "bye"}))
            .await
            .unwrap(),
        ClientSendOutcome::Sent { .. }
    ));

    let owner = AccessLogOwner::Server(sid.as_u32());
    let w = Duration::from_secs(20);
    let init = &logs(&state, owner, "bmp_initiation", 1, w).await[0].request;
    assert_eq!(
        (init["sys_name"].as_str(), init["sys_descr"].as_str()),
        (Some("edge-3"), Some("lab"))
    );
    let up = &logs(&state, owner, "bmp_peer_up", 1, w).await[0].request;
    assert_eq!(up["peer"]["type"], "rd_instance");
    assert_eq!(up["peer"]["distinguisher"], "65000:5");
    assert_eq!(up["peer"]["post_policy"], true);
    assert_eq!(
        up["sent_open"],
        json!({"asn": 65001, "hold_time": 30, "bgp_id": "192.0.2.1", "capabilities": ["four_octet_as(65001)"]})
    );
    assert_eq!(up["received_open"]["asn"], 4200000002u64);
    assert_eq!(
        (
            up["local_address"].as_str(),
            up["remote_port"].as_u64(),
            up["local_port"].as_u64()
        ),
        (Some("192.0.2.1"), Some(40179), Some(179))
    );
    let rm = logs(&state, owner, "bmp_route_monitoring", 2, w).await;
    let u = &rm[0].request["update"];
    assert_eq!(
        (
            u["nlri"].clone(),
            u["as_path"].clone(),
            u["origin"].as_str()
        ),
        (
            json!(["203.0.113.0/24"]),
            json!([4200000002u64, 65010]),
            Some("EGP")
        ),
        "{u}"
    );
    let attrs = u["path_attributes"].as_array().unwrap();
    assert!(
        attrs
            .iter()
            .any(|a| a["communities"] == json!(["65002:100", "0:7"])),
        "{u}"
    );
    assert!(attrs.iter().any(|a| a["local_pref"] == 150) && attrs.iter().any(|a| a["med"] == 20));
    assert_eq!(
        rm[1].request["update"]["withdrawn_routes"],
        json!(["203.0.113.0/24"])
    );
    let stats = &logs(&state, owner, "bmp_statistics", 1, w).await[0].request;
    assert_eq!(
        stats["counters"],
        json!([
            {"type": "rejected_prefixes", "type_code": 0, "value": 3},
            {"type": "loc_rib_routes", "type_code": 8, "value": 5000000000u64},
            {"type": "adj_rib_in_routes_per_afi_safi", "type_code": 9, "afi": 1, "safi": 1, "value": 9}
        ])
    );
    let down = &logs(&state, owner, "bmp_peer_down", 1, w).await[0].request;
    assert_eq!(
        down["notification"]["subcode_name"], "Administrative Shutdown",
        "{down}"
    );
    let term = &logs(&state, owner, "bmp_termination", 1, w).await[0].request;
    assert_eq!(
        (
            term["reason"].as_str(),
            term["strings"].clone(),
            term["router"].as_str()
        ),
        (Some("out_of_resources"), json!(["bye"]), Some("edge-3"))
    );
    state.remove_client(cid).await;
    state.remove_server(sid).await;
}

async fn closed(stream: &mut TcpStream) -> bool {
    let mut b = [0u8; 16];
    matches!(
        tokio::time::timeout(Duration::from_secs(10), stream.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn collector_refusals() {
    let state = state();
    let (sid, collector) = server_in(&state, policy()).await;
    let owner = AccessLogOwner::Server(sid.as_u32());
    let init = |name: &str| codec::initiation(name, "raw", &[]).unwrap();

    // Not starting with Initiation.
    let mut s = TcpStream::connect(collector).await.unwrap();
    s.write_all(&codec::termination(0, None).unwrap())
        .await
        .unwrap();
    assert!(closed(&mut s).await);
    // BMP version 2.
    let mut s = TcpStream::connect(collector).await.unwrap();
    let mut v2 = init("old");
    v2[0] = 2;
    s.write_all(&v2).await.unwrap();
    assert!(closed(&mut s).await);
    // A length over the bound, refused from the header alone.
    let mut s = TcpStream::connect(collector).await.unwrap();
    let mut big = vec![3];
    big.extend(((codec::MAX_MESSAGE + 1) as u32).to_be_bytes());
    big.push(0);
    s.write_all(&big).await.unwrap();
    assert!(closed(&mut s).await);
    // A route monitoring message whose per-peer header is cut short.
    let mut s = TcpStream::connect(collector).await.unwrap();
    s.write_all(&init("short")).await.unwrap();
    s.write_all(&codec::frame(codec::ROUTE_MONITORING, &[0; 10]))
        .await
        .unwrap();
    assert!(closed(&mut s).await);
    // A router the handler refuses.
    let mut s = TcpStream::connect(collector).await.unwrap();
    s.write_all(&init("blocked")).await.unwrap();
    assert!(closed(&mut s).await);

    let names: Vec<_> = logs(&state, owner, "bmp_initiation", 2, Duration::from_secs(10))
        .await
        .into_iter()
        .map(|e| {
            e.request["sys_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    assert_eq!(names, ["short", "blocked"]);
    let all = state.list_access_logs_for(Some(owner), None).await;
    assert!(
        all.iter().all(|e| e.event_type == "bmp_initiation"),
        "{:?}",
        all.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    state.remove_server(sid).await;
}
