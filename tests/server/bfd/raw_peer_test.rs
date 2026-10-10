//! NetGet's BFD speaker against a peer written in the test, for what BIRD cannot be made to
//! do: send with the wrong TTL, stop mid-session, oversize a datagram. Every packet NetGet
//! sent is then read by tshark's BFD dissector (the pcap oracle). No LLM calls.
use crate::helpers::pcap_oracle::PcapOracle;
use netget::cli::management::ServerForm;
use netget::server::bfd::packet::{self, ControlPacket, State};
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
a=[]
if t=='bfd_session_request' and e['peer'] in ('127.0.0.5','127.0.0.6'):
  a=[{'type':'bfd_accept_session','desired_min_tx_ms':100,'required_min_rx_ms':100,'detect_mult':3}]
print(json.dumps({'actions':a}))"#;

async fn start(params: Value) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "bfd".into(),
        port: Some(0),
        host: Some("127.0.0.2".into()),
        instruction: Some("Accept 127.0.0.5 and 127.0.0.6".into()),
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

/// A peer on `ip`, listening where NetGet sends: the server's own port number.
struct Peer {
    socket: UdpSocket,
    server: (&'static str, u16),
    sent: Vec<Vec<u8>>,
    received: Vec<Vec<u8>>,
}

impl Peer {
    async fn new(ip: &str, port: u16) -> Self {
        let socket = UdpSocket::bind((ip, port)).await.unwrap();
        socket.set_ttl(255).unwrap();
        Self {
            socket,
            server: ("127.0.0.2", port),
            sent: vec![],
            received: vec![],
        }
    }
    async fn send(&mut self, p: &ControlPacket) {
        let bytes = p.encode(None);
        self.socket.send_to(&bytes, self.server).await.unwrap();
        self.sent.push(bytes);
    }
    async fn send_raw(&mut self, bytes: &[u8]) {
        self.socket.send_to(bytes, self.server).await.unwrap();
    }
    /// The next packet NetGet sends that satisfies `pred`, within `secs`.
    async fn expect(
        &mut self,
        secs: u64,
        pred: impl Fn(&ControlPacket) -> bool,
    ) -> Option<ControlPacket> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        let mut buf = [0u8; 256];
        loop {
            let n = match tokio::time::timeout_at(deadline, self.socket.recv_from(&mut buf)).await {
                Ok(r) => r.unwrap().0,
                Err(_) => return None,
            };
            self.received.push(buf[..n].to_vec());
            let p =
                packet::decode(&buf[..n]).expect("NetGet sent a packet its own decoder refuses");
            if pred(&p) {
                return Some(p);
            }
        }
    }
}

fn control(state: State, mine: u32, yours: u32) -> ControlPacket {
    ControlPacket {
        diag: 0,
        state,
        poll: false,
        final_: false,
        control_plane_independent: false,
        demand: false,
        detect_mult: 3,
        my_discriminator: mine,
        your_discriminator: yours,
        desired_min_tx_us: 100_000,
        required_min_rx_us: 100_000,
        required_min_echo_rx_us: 0,
        auth: None,
    }
}

#[tokio::test]
async fn ttl_handshake_poll_detection_and_oracle() {
    let (state, id, port) = start(json!({})).await;
    let mut peer = Peer::new("127.0.0.5", port).await;

    // Single-hop: a packet that crossed a router (TTL below 255) is not BFD from a neighbour.
    peer.socket.set_ttl(64).unwrap();
    peer.send(&control(State::Down, 0x1111, 0)).await;
    assert!(
        peer.expect(2, |_| true).await.is_none(),
        "answered a TTL-64 packet"
    );
    assert!(events(&state, id, "bfd_session_request").await.is_empty());

    // TTL 255: accepted, and NetGet answers Init naming our discriminator.
    peer.socket.set_ttl(255).unwrap();
    peer.send(&control(State::Down, 0x1111, 0)).await;
    let init = peer
        .expect(10, |p| p.state == State::Init)
        .await
        .expect("NetGet never went Init");
    assert_eq!(init.your_discriminator, 0x1111);
    assert_eq!(
        init.desired_min_tx_us, 1_000_000,
        "a session not Up sends at most once a second"
    );
    let theirs = init.my_discriminator;

    peer.send(&control(State::Up, 0x1111, theirs)).await;
    let up = peer
        .expect(5, |p| p.state == State::Up)
        .await
        .expect("never Up");
    // Up, NetGet lowers its transmit interval to the 100 ms it was given — by Poll Sequence.
    assert!(up.poll || peer.expect(5, |p| p.poll).await.is_some());
    let mut fin = control(State::Up, 0x1111, theirs);
    fin.final_ = true;
    peer.send(&fin).await;
    let mut poll = control(State::Up, 0x1111, theirs);
    poll.poll = true;
    peer.send(&poll).await;
    let answer = peer
        .expect(5, |p| p.final_)
        .await
        .expect("a Poll was not answered with Final");
    assert!(!answer.poll, "a packet with both Poll and Final");
    assert_eq!(answer.desired_min_tx_us, 100_000);

    // Silence: Down after the Detection Time (3 × 100 ms), with diagnostic 1. A passive
    // speaker that has forgotten the remote discriminator may not transmit (§6.8.7), so the
    // event is where this shows.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let states = events(&state, id, "bfd_session_state").await;
        if let Some(down) = states
            .iter()
            .find(|e| e["state"] == "Down" && e["previous_state"] == "Up")
        {
            assert_eq!(down["diag"], "control_detection_time_expired", "{down}");
            assert_eq!(
                down["remote_discriminator"], 0,
                "the remote discriminator is forgotten"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never noticed the silence: {states:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        // (Up packets sent before the expiry may still be queued; nothing after it is sent.)
        peer.expect(1, |p| p.state != State::Up).await.is_none(),
        "a passive speaker with no remote discriminator transmitted"
    );

    // tshark's dissector over every packet NetGet sent (and ours, for context).
    let mut oracle = PcapOracle::udp("bfd").port(packet::SINGLE_HOP_PORT);
    for bytes in &peer.sent {
        oracle = oracle.to_server(bytes);
    }
    for bytes in &peer.received {
        oracle = oracle.from_server(bytes);
    }
    oracle.assert_clean();
}

#[tokio::test]
async fn bounds_and_refusals() {
    let (state, id, port) = start(json!({"max_sessions": 1})).await;
    let mut a = Peer::new("127.0.0.5", port).await;

    // Longer than any Control packet: dropped before it is parsed; the server goes on.
    let mut big = control(State::Down, 0x2222, 0).encode(None);
    big.resize(packet::MAX_PACKET + 1, 0);
    a.send_raw(&big).await;
    a.send(&control(State::Down, 0x2222, 0)).await;
    a.expect(10, |p| p.state == State::Init)
        .await
        .expect("A not accepted");

    // One session is the cap: B is never asked about.
    let mut b = Peer::new("127.0.0.6", port).await;
    b.send(&control(State::Down, 0x3333, 0)).await;
    assert!(b.expect(2, |_| true).await.is_none());

    // A peer the model declines is asked about once, not once per packet.
    let (state2, id2, port2) = start(json!({})).await;
    let mut c = Peer::new("127.0.0.7", port2).await;
    for _ in 0..3 {
        c.send(&control(State::Down, 0x4444, 0)).await;
        assert!(c.expect(1, |_| true).await.is_none());
    }
    assert_eq!(events(&state2, id2, "bfd_session_request").await.len(), 1);
    let asked: Vec<_> = events(&state, id, "bfd_session_request")
        .await
        .into_iter()
        .map(|e| e["peer"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(asked, ["127.0.0.5"], "{asked:?}");
}
