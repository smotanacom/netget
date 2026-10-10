//! NetGet's VXLAN/Geneve endpoint against a tunnel endpoint written in the test, for both
//! encapsulations, what the kernel test cannot cover (Geneve: this kernel has no geneve
//! module), and the refusals. tshark's VXLAN and Geneve dissectors read every frame both sides
//! sent (the pcap oracle). Unprivileged; no LLM calls.
use crate::helpers::pcap_oracle::PcapOracle;
use netget::cli::management::ServerForm;
use netget::server::vxlan::frame::{self, Encap, Payload};
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='vxlan_arp_request' and e['target_ip']=='10.99.0.2':
  a=[{'type':'vxlan_arp_reply','mac':'02:4e:47:00:00:02'}]
elif t=='vxlan_icmp_echo_request':
  a=[{'type':'vxlan_icmp_echo_reply'}]
elif t=='vxlan_udp_datagram':
  a=[{'type':'vxlan_udp_reply','data':'pong:'+e['data']}]
print(json.dumps({'actions':a}))"#;

const PEER_MAC: frame::Mac = [0x02, 0, 0, 0, 0, 0x01];
const PEER_IP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 1);
const HOST_IP: Ipv4Addr = Ipv4Addr::new(10, 99, 0, 2);
const HOST_MAC: frame::Mac = [0x02, 0x4e, 0x47, 0, 0, 0x02];

async fn start(params: Value) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "vxlan".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Be 10.99.0.2".into()),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
        ]),
        startup_params: Some(params),
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
    (state, id, port)
}

async fn events(state: &AppState, id: ServerId, event_type: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == event_type)
        .map(|e| e["request"].clone())
        .collect()
}

/// The test's VTEP, on 127.0.0.9 at the server's own port: where NetGet replies.
struct Vtep {
    socket: UdpSocket,
    port: u16,
    encap: Encap,
    vni: u32,
    sent: Vec<Vec<u8>>,
    received: Vec<Vec<u8>>,
}

impl Vtep {
    async fn new(port: u16, encap: Encap, vni: u32) -> Self {
        Self {
            socket: UdpSocket::bind(("127.0.0.9", port)).await.unwrap(),
            port,
            encap,
            vni,
            sent: vec![],
            received: vec![],
        }
    }
    async fn send(&mut self, inner: &[u8]) {
        let d = frame::encap(self.encap, self.vni, inner);
        self.socket
            .send_to(&d, ("127.0.0.1", self.port))
            .await
            .unwrap();
        self.sent.push(d);
    }
    async fn send_raw(&mut self, d: &[u8]) {
        self.socket
            .send_to(d, ("127.0.0.1", self.port))
            .await
            .unwrap();
    }
    async fn recv(&mut self, secs: u64) -> Option<frame::Frame> {
        let mut buf = vec![0u8; 65_536];
        let n = tokio::time::timeout(Duration::from_secs(secs), self.socket.recv_from(&mut buf))
            .await
            .ok()?
            .unwrap()
            .0;
        self.received.push(buf[..n].to_vec());
        let (vni, inner) = frame::decap(self.encap, &buf[..n]).expect("NetGet's encapsulation");
        assert_eq!(vni, self.vni);
        Some(frame::parse_frame(inner).expect("NetGet's inner frame (checksums included)"))
    }
}

async fn exchange(encap: Encap, dissector: &str, oracle_port: u16) {
    let (state, id, port) = start(json!({"encapsulation": encap.name(), "vni": 42})).await;
    let mut v = Vtep::new(port, encap, 42).await;

    v.send(&frame::arp_frame(true, PEER_MAC, PEER_IP, [0; 6], HOST_IP))
        .await;
    let arp = v.recv(20).await.expect("no ARP reply");
    let Payload::Arp(a) = &arp.payload else {
        panic!("{arp:?}")
    };
    assert!(!a.request);
    assert_eq!(
        (a.sender_mac, a.sender_ip, a.target_mac, a.target_ip),
        (HOST_MAC, HOST_IP, PEER_MAC, PEER_IP)
    );
    assert_eq!(arp.dst_mac, PEER_MAC);

    let ping = frame::ipv4(
        PEER_IP,
        HOST_IP,
        1,
        7,
        &frame::icmp_echo(true, 0x1234, 1, b"abcdefgh"),
    );
    v.send(&frame::ethernet(
        HOST_MAC,
        PEER_MAC,
        frame::ETHERTYPE_IPV4,
        &ping,
    ))
    .await;
    let reply = v.recv(20).await.expect("no echo reply");
    assert_eq!(
        (reply.src_ip, reply.dst_ip, reply.src_mac),
        (Some(HOST_IP), Some(PEER_IP), HOST_MAC)
    );
    assert_eq!(
        reply.payload,
        Payload::Echo {
            request: false,
            identifier: 0x1234,
            sequence: 1,
            data: b"abcdefgh".to_vec()
        }
    );

    let udp = frame::ipv4(
        PEER_IP,
        HOST_IP,
        17,
        8,
        &frame::udp(PEER_IP, HOST_IP, 5000, 7777, b"hello"),
    );
    v.send(&frame::ethernet(
        HOST_MAC,
        PEER_MAC,
        frame::ETHERTYPE_IPV4,
        &udp,
    ))
    .await;
    let answer = v.recv(20).await.expect("no UDP reply");
    assert_eq!(
        answer.payload,
        Payload::Udp {
            src_port: 7777,
            dst_port: 5000,
            data: b"pong:hello".to_vec()
        }
    );

    let mut oracle = PcapOracle::udp(dissector).port(oracle_port);
    for d in &v.sent {
        oracle = oracle.to_server(d);
    }
    for d in &v.received {
        oracle = oracle.from_server(d);
    }
    oracle.assert_clean();
    assert_eq!(events(&state, id, "vxlan_udp_datagram").await[0]["vni"], 42);
}

#[tokio::test]
async fn vxlan_arp_ping_udp_and_oracle() {
    exchange(Encap::Vxlan, "vxlan", frame::VXLAN_PORT).await;
}

#[tokio::test]
async fn geneve_arp_ping_udp_and_oracle() {
    exchange(Encap::Geneve, "geneve", frame::GENEVE_PORT).await;
}

#[tokio::test]
async fn refusals_and_the_vni_filter() {
    let (state, id, port) = start(json!({"vni": 42})).await;
    let mut v = Vtep::new(port, Encap::Vxlan, 42).await;
    let arp = frame::arp_frame(true, PEER_MAC, PEER_IP, [0; 6], HOST_IP);

    // The I flag clear: no valid VNI, dropped.
    let mut no_i = frame::encap(Encap::Vxlan, 42, &arp);
    no_i[0] = 0;
    v.send_raw(&no_i).await;
    // Another VNI: not this server's overlay.
    v.send_raw(&frame::encap(Encap::Vxlan, 43, &arp)).await;
    // A truncated inner frame, and an IPv4 header with a wrong checksum.
    v.send_raw(&frame::encap(Encap::Vxlan, 42, &arp[..20]))
        .await;
    let mut bad = frame::ipv4(PEER_IP, HOST_IP, 1, 7, &frame::icmp_echo(true, 1, 1, b"x"));
    bad[10] ^= 0xff;
    v.send_raw(&frame::encap(
        Encap::Vxlan,
        42,
        &frame::ethernet(HOST_MAC, PEER_MAC, frame::ETHERTYPE_IPV4, &bad),
    ))
    .await;
    assert!(
        v.recv(2).await.is_none(),
        "answered something it should have dropped"
    );
    // None of them reached the model; and the server still answers.
    assert!(events(&state, id, "vxlan_arp_request").await.is_empty());
    assert!(events(&state, id, "vxlan_icmp_echo_request")
        .await
        .is_empty());
    v.send(&arp).await;
    assert!(v.recv(20).await.is_some());

    // An IP with no host: silence (the policy answers ARP for 10.99.0.2 only).
    v.send(&frame::arp_frame(
        true,
        PEER_MAC,
        PEER_IP,
        [0; 6],
        Ipv4Addr::new(10, 99, 0, 3),
    ))
    .await;
    assert!(v.recv(2).await.is_none());
}
