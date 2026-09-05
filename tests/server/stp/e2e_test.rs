//! STP / RSTP end-to-end: a real frame in, a decision, a real frame out — unprivileged.
//!
//! # Why this does not go through the harness
//!
//! Every other server suite starts the `netget` binary and lets `server_startup` bring the
//! protocol up. That cannot work here, and the reason is worth knowing before you try:
//! **`server_startup`'s privilege gate is per-protocol, not per-transport.** STP declares
//! `PrivilegeRequirement::RawSockets`, and `requires_privileges` is `!privilege_met` for that
//! variant, so an unprivileged `start_server` is refused *before* the startup parameters are
//! read — including `transport: "udp"`, which needs no privilege at all. Declaring anything
//! weaker would be a lie about the raw transport, which is the real one.
//!
//! So these tests call `Server::spawn(ctx)` directly with a hand-built `SpawnContext`. That
//! still exercises everything this protocol owns: startup-parameter parsing, the bind, the
//! 802.3 decode, the event, the handler/LLM dispatch, action execution and the re-encode. Only
//! `server_startup`'s own gate is bypassed, and that gate is not this protocol's code.
//!
//! # What the UDP transport is
//!
//! One complete 802.3 + LLC BPDU frame per datagram — the same octets the raw transport would
//! put on the segment, with the link layer simulated. The frames below are the same literals
//! `codec_test.rs` checks against the specification, so what crosses the socket here is
//! independently pinned bytes rather than whatever the encoder happens to produce.
//!
//! LLM call budget: 3.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features stp \
//!       --test server -- stp:: --test-threads=100

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::helpers::mock_builder::MockLlmBuilder;
use crate::helpers::mock_ollama::MockOllamaServer;

use netget::llm::actions::protocol_trait::{Protocol, Server as ServerProtocol};
use netget::llm::ollama_client::OllamaClient;
use netget::protocol::{SpawnContext, StartupParams};
use netget::server::stp::actions::StpProtocol;
use netget::server::stp::codec::{self, Bpdu};
use netget::state::app_state::AppState;
use netget::state::server::ServerInstance;
use netget::state::ServerId;

/// The bridge this server is configured to be. Priority 4096 is deliberately *not* the default
/// 32768, so an assertion on it proves the startup parameter was read rather than that a
/// constant happened to match.
const LOCAL_BRIDGE_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const LOCAL_BRIDGE_PRIORITY: u16 = 4096;

const BRIDGE_GROUP_ADDRESS: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x00];

/// The peer's MAC in the frames below.
const PEER_MAC: [u8; 6] = [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f];

/// An 802.1D configuration BPDU frame — the same literal `codec_test.rs` pins against
/// IEEE 802.1D-2004 §9.3.1. A neighbouring bridge with priority 32768 claiming to be root.
const CONFIG_BPDU_FRAME: [u8; 60] = [
    0x01, 0x80, 0xc2, 0x00, 0x00, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f, 0x00, 0x26, 0x42, 0x42,
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x80, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00, 0x80, 0x04, 0x00, 0x00, 0x14, 0x00,
    0x02, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// A Topology Change Notification BPDU frame (802.1D-2004 §9.3.2).
const TCN_BPDU_FRAME: [u8; 60] = [
    0x01, 0x80, 0xc2, 0x00, 0x00, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f, 0x00, 0x07, 0x42, 0x42,
    0x03, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

struct Harness {
    /// Kept alive for the duration of the test: dropping the state would drop the server task.
    _state: Arc<AppState>,
    addr: SocketAddr,
    status_rx: UnboundedReceiver<String>,
}

/// Bring an STP server up on the UDP transport, pointed at `ollama_url`.
///
/// `ollama_url` is the only knob the silence test needs: pointing it at a closed port is how
/// an LLM failure is produced without a mock that has to be told to misbehave.
#[allow(deprecated)] // SpawnContext::listen_addr is the legacy binding field; still required.
async fn start_stp_over_udp(ollama_url: String, instruction: &str) -> Harness {
    let state = Arc::new(AppState::new_with_options(false, ollama_url.clone()));
    // Pin the model so `ensure_model_selected` never falls back to probing a real Ollama on
    // localhost:11434, which would make the test depend on the developer's machine.
    state.set_ollama_model(Some("mock-model".to_string())).await;
    state
        .set_llm_client(OllamaClient::new(ollama_url.clone()))
        .await;

    let server_id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "STP".to_string(),
            instruction.to_string(),
        ))
        .await;

    let protocol = StpProtocol::new();
    let startup_params = StartupParams::new(
        serde_json::json!({
            "transport": "udp",
            "bridge_mac": "02:00:00:00:00:01",
            "bridge_priority": LOCAL_BRIDGE_PRIORITY,
            "system_id_extension": 0,
            "port_priority": 128,
            "port_number": 1,
            "protocol_version": "rstp",
            "hello_time": 2,
            "max_age": 20,
            "forward_delay": 15,
        }),
        protocol.get_startup_parameters(),
    )
    .expect("every parameter the test passes must be declared by the protocol");

    let (status_tx, status_rx) = mpsc::unbounded_channel();
    let ctx = SpawnContext {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        mac_address: None,
        interface: None,
        host: Some("127.0.0.1".to_string()),
        port: Some(0),
        llm_client: OllamaClient::new(ollama_url),
        state: state.clone(),
        status_tx,
        server_id,
        startup_params: Some(startup_params),
    };

    let addr = protocol
        .spawn(ctx)
        .await
        .expect("STP must start on the UDP transport without privilege");
    assert_ne!(addr.port(), 0, "spawn must report the bound port");

    Harness {
        _state: state,
        addr,
        status_rx,
    }
}

/// A client socket already connected to the server, so `send`/`recv` need no address.
async fn peer_socket(server: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    socket.connect(server).await.expect("connect to STP server");
    socket
}

/// Wait up to `secs` for one frame back.
async fn recv_frame(socket: &UdpSocket, secs: u64) -> Option<Vec<u8>> {
    let mut buffer = vec![0u8; 2048];
    match tokio::time::timeout(Duration::from_secs(secs), socket.recv(&mut buffer)).await {
        Ok(Ok(n)) => Some(buffer[..n].to_vec()),
        _ => None,
    }
}

/// Wait for a status line containing `needle`, returning it. `None` on timeout.
async fn wait_for_status(
    rx: &mut UnboundedReceiver<String>,
    needle: &str,
    secs: u64,
) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
            Ok(Some(line)) => {
                if line.contains(needle) {
                    return Some(line);
                }
            }
            Ok(None) => return None,
            Err(_) => continue,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The full path
// ---------------------------------------------------------------------------

/// A configuration BPDU arrives, the model is asked, and what it decides goes back on the
/// wire as a real RST BPDU.
///
/// The model is told to claim the root bridge with priority 0 — the actual attack this
/// protocol makes possible — and the assertion is that the frame coming back really carries
/// priority 0 in the root identifier, at the offset the specification puts it.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bpdu_produces_the_bpdu_the_model_decided_on() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("stp_bpdu_received")
            .respond_with_actions(serde_json::json!([{
                "type": "send_stp_bpdu",
                "protocol_version": "rstp",
                "root_priority": 0,
                "root_bridge_mac": "02:00:00:00:00:01",
                "root_system_id_extension": 0,
                "root_path_cost": 0,
                "port_role": "designated",
                "learning": true,
                "forwarding": true
            }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let harness = start_stp_over_udp(
        mock.base_url(),
        "You are a spanning tree bridge. Claim the root bridge with priority 0.",
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&CONFIG_BPDU_FRAME).await.expect("send BPDU");

    let reply = recv_frame(&peer, 30)
        .await
        .expect("the server must answer with the BPDU the model asked for");

    let frame = codec::decode_frame(&reply).expect("the reply must be a valid 802.3 BPDU frame");
    assert_eq!(
        frame.destination, BRIDGE_GROUP_ADDRESS,
        "BPDUs go to the Bridge Group Address unless the action says otherwise"
    );
    assert_eq!(
        frame.source, LOCAL_BRIDGE_MAC,
        "the source MAC must come from the bridge_mac startup parameter"
    );
    assert_ne!(
        frame.source, PEER_MAC,
        "the reply is this bridge speaking, not the sender's frame echoed back"
    );

    let Bpdu::Config(bpdu) = Bpdu::decode(&frame.payload).expect("decode the reply BPDU") else {
        panic!("the model asked for a configuration-shaped BPDU, not a TCN");
    };

    assert!(
        bpdu.is_rstp(),
        "protocol_version 'rstp' must produce an RST BPDU (version 2, type 0x02)"
    );
    assert_eq!(
        bpdu.root.priority, 0,
        "the model claimed root priority 0 and that is what must be on the wire — this is the \
         field that re-converges somebody's spanning tree"
    );
    assert_eq!(bpdu.root.mac_string(), "02:00:00:00:00:01");
    assert_eq!(bpdu.root_path_cost, 0);
    assert_eq!(
        bpdu.flags.port_role,
        codec::PortRole::Designated,
        "port_role 'designated' must reach the flags octet"
    );
    assert!(bpdu.flags.learning && bpdu.flags.forwarding);

    // Fields the model did not name must come from the startup parameters, not from constants.
    assert_eq!(
        bpdu.bridge.priority, LOCAL_BRIDGE_PRIORITY,
        "the configured bridge_priority must fill in what the action omitted"
    );
    assert_eq!(bpdu.bridge.mac_string(), "02:00:00:00:00:01");
    assert_eq!(bpdu.port.priority, 128);
    assert_eq!(bpdu.port.number, 1);
    assert_eq!(bpdu.max_age_seconds, 20.0);
    assert_eq!(bpdu.hello_time_seconds, 2.0);
    assert_eq!(bpdu.forward_delay_seconds, 15.0);

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// A Topology Change Notification raises `stp_topology_change`, not `stp_bpdu_received`, and
/// the model can answer it with a TCN of its own.
///
/// The two events exist because they are different questions: one is "here is the current
/// topology", the other is "the topology just changed". Routing a TCN to the wrong event would
/// make an operator's handler for it never match.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topology_change_notification_raises_the_topology_change_event() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("stp_topology_change")
            .respond_with_actions(serde_json::json!([{ "type": "send_stp_tcn" }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let harness = start_stp_over_udp(
        mock.base_url(),
        "Propagate topology changes towards the root.",
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&TCN_BPDU_FRAME).await.expect("send TCN");

    let reply = recv_frame(&peer, 30)
        .await
        .expect("the server must answer the topology change with the TCN the model asked for");
    let frame = codec::decode_frame(&reply).expect("valid 802.3 frame");
    assert_eq!(frame.source, LOCAL_BRIDGE_MAC);
    assert_eq!(
        Bpdu::decode(&frame.payload).expect("decode"),
        Bpdu::TopologyChangeNotification
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// `no_bpdu` is a real answer: nothing goes on the wire, and the log says the model chose it.
///
/// This matters because on the wire it is indistinguishable from every other silence. The
/// `decision=model_reject` tag is the only place the difference survives, exactly as `radius`
/// separates a model denial from a fail-closed default.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_bpdu_transmits_nothing_and_is_logged_as_a_decision() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("stp_bpdu_received")
            .respond_with_actions(serde_json::json!([{ "type": "no_bpdu" }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let mut harness = start_stp_over_udp(
        mock.base_url(),
        "Observe the spanning tree. Do not take part in it.",
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&CONFIG_BPDU_FRAME).await.expect("send BPDU");

    let decision = wait_for_status(&mut harness.status_rx, "decision=model_reject", 30)
        .await
        .expect(
            "an explicit no_bpdu must be logged as decision=model_reject — without the tag it \
             is indistinguishable from a backend failure",
        );
    assert!(decision.contains("no BPDU"), "got: {decision}");

    assert!(
        recv_frame(&peer, 3).await.is_none(),
        "no_bpdu means no frame. Anything on the wire here is a BPDU nobody decided to send."
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// **The silence test.** When the LLM call fails, nothing goes on the wire.
///
/// STP is in the deliberately-silent class and its case is among the strongest in the tree: a
/// BPDU is a positive assertion about topology, and a fabricated one does not merely mislead a
/// peer — it can make every switch on the segment recompute the spanning tree and stop
/// forwarding while it does. There is no error BPDU to send instead, so the only correct
/// answer is nothing.
///
/// A bare "no frame arrived" would prove nothing on its own: it is equally consistent with a
/// server that never received the frame. So this asserts the pair — the `decision=fail_closed_`
/// line proves the frame *was* decoded, the event *was* raised and the model *was* asked, and
/// the absent frame proves the failure produced no wire output. `a_bpdu_produces_the_bpdu_the_
/// model_decided_on` above is the positive control for the same path.
///
/// LLM calls: 0 reach a mock; the backend is a closed port on purpose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_llm_failure_puts_nothing_on_the_wire() {
    // Port 1 is not listening. Nothing else about the server changes.
    let mut harness = start_stp_over_udp(
        "http://127.0.0.1:1".to_string(),
        "You are a spanning tree bridge. Claim the root bridge with priority 0.",
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&CONFIG_BPDU_FRAME).await.expect("send BPDU");

    let decision = wait_for_status(&mut harness.status_rx, "decision=fail_closed_", 90)
        .await
        .expect(
            "the LLM failure must be recorded with a decision= tag. If this times out the \
             event never reached dispatch and the assertion below proves nothing.",
        );
    assert!(
        decision.contains("nothing transmitted"),
        "the operator log must say plainly that no BPDU was sent, got: {decision}"
    );

    assert!(
        recv_frame(&peer, 5).await.is_none(),
        "an LLM failure must put NOTHING on the wire. A fabricated BPDU here would assert a \
         root bridge netget cannot back and can re-converge a real network segment."
    );
}

// ---------------------------------------------------------------------------
// Startup parameters and the raw transport
// ---------------------------------------------------------------------------

/// Every parameter the implementation reads is declared, and nothing else is accepted.
///
/// `StartupParams::new` validates against `get_startup_parameters()`, so an undeclared key is
/// a clean error naming the allowed set rather than a silently ignored knob.
#[test]
fn the_declared_startup_parameters_are_exactly_the_ones_the_server_reads() {
    let declared: std::collections::BTreeSet<String> = StpProtocol::new()
        .get_startup_parameters()
        .into_iter()
        .map(|p| p.name)
        .collect();

    let expected: std::collections::BTreeSet<String> = [
        "transport",
        "bridge_mac",
        "bridge_priority",
        "system_id_extension",
        "port_priority",
        "port_number",
        "protocol_version",
        "hello_time",
        "max_age",
        "forward_delay",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    assert_eq!(
        declared, expected,
        "the declared parameter set drifted from the set StpBridgeConfig::from_startup_params \
         reads. A declared parameter nothing reads is an advertised knob that does nothing."
    );

    let err = StartupParams::new(
        serde_json::json!({ "bridge_prioritee": 0 }),
        StpProtocol::new().get_startup_parameters(),
    )
    .expect_err("an undeclared parameter must be refused, not ignored");
    assert!(format!("{err}").contains("bridge_prioritee"));
}

/// A bridge priority that cannot be encoded refuses at startup rather than failing on every
/// BPDU it later tries to send.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(deprecated)]
async fn an_unencodable_bridge_priority_refuses_to_start() {
    let state = Arc::new(AppState::new_with_options(
        false,
        "http://127.0.0.1:1".into(),
    ));
    let server_id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "STP".to_string(),
            "bridge".to_string(),
        ))
        .await;
    let (status_tx, _rx) = mpsc::unbounded_channel();

    let protocol = StpProtocol::new();
    let params = StartupParams::new(
        // 32769 is what you get from reading the packed field 0x8001 as one number. The low
        // 12 bits are the VLAN, so there is nowhere to put the extra 1.
        serde_json::json!({ "transport": "udp", "bridge_priority": 32769 }),
        protocol.get_startup_parameters(),
    )
    .expect("the key itself is declared");

    let err = protocol
        .spawn(SpawnContext {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mac_address: None,
            interface: None,
            host: Some("127.0.0.1".to_string()),
            port: Some(0),
            llm_client: OllamaClient::new("http://127.0.0.1:1"),
            state,
            status_tx,
            server_id,
            startup_params: Some(params),
        })
        .await
        .expect_err("a priority that is not a multiple of 4096 cannot be put on the wire");

    let message = format!("{err:#}");
    assert!(
        message.contains("4096"),
        "the refusal must explain the 4096 step, got: {message}"
    );
}

/// The raw transport must never report success without the capture handle it needs.
///
/// This is the ARP/DataLink/ICMP/IS-IS regression the root `CLAUDE.md` records: a `spawn` that
/// fires the privileged open off into `spawn_blocking` and returns `Ok` before the result is
/// known, leaving a server in `Running` that has captured nothing, forever. STP is written
/// with the readiness handshake from the start; this is what keeps it that way.
///
/// Two branches, both meaningful on any host:
///
/// * a device that does not exist — always `Err`, whatever the privilege, because the lookup
///   happens before the open;
/// * loopback **without** capture privilege — must be `Err` naming the missing privilege. This
///   is the branch every developer machine and every CI runner takes. It is skipped when the
///   host *does* have capture access, because Linux presents `lo` with a synthetic Ethernet
///   header, so `ether dst` compiles there and a privileged spawn legitimately succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(deprecated)]
async fn the_raw_transport_never_reports_success_without_a_capture_handle() {
    async fn spawn_raw_on(interface: &str) -> anyhow::Result<SocketAddr> {
        let state = Arc::new(AppState::new_with_options(
            false,
            "http://127.0.0.1:1".to_string(),
        ));
        let server_id = state
            .add_server(ServerInstance::new(
                ServerId::new(0),
                0,
                "STP".to_string(),
                "bridge".to_string(),
            ))
            .await;
        let (status_tx, _rx) = mpsc::unbounded_channel();
        let protocol = StpProtocol::new();
        let params = StartupParams::new(
            serde_json::json!({ "transport": "raw" }),
            protocol.get_startup_parameters(),
        )
        .unwrap();

        protocol
            .spawn(SpawnContext {
                listen_addr: "127.0.0.1:0".parse().unwrap(),
                mac_address: None,
                interface: Some(interface.to_string()),
                host: Some("127.0.0.1".to_string()),
                port: Some(0),
                llm_client: OllamaClient::new("http://127.0.0.1:1"),
                state,
                status_tx,
                server_id,
                startup_params: Some(params),
            })
            .await
    }

    match spawn_raw_on("netget-no-such-device0").await {
        Ok(addr) => panic!(
            "STP raw spawn returned Ok({addr}) for a device that does not exist. No capture \
             handle can have opened, so this server would sit in Running having captured \
             nothing — the fire-and-forget spawn_blocking regression."
        ),
        Err(e) => {
            let message = format!("{e:#}");
            assert!(
                message.contains("netget-no-such-device0")
                    && message.contains("no such capture device"),
                "the refusal must name the device that could not be opened, got: {message}"
            );
        }
    }

    if netget::privilege::SystemCapabilities::detect().has_packet_capture_access {
        // A privileged host can legitimately open loopback on Linux, where pcap presents `lo`
        // with a synthetic Ethernet header. Nothing to assert that would hold everywhere.
        return;
    }

    let loopback = if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )) {
        "lo0"
    } else {
        "lo"
    };

    match spawn_raw_on(loopback).await {
        Ok(addr) => panic!(
            "STP raw spawn returned Ok({addr}) on '{loopback}' without capture privilege. The \
             pcap handle cannot have opened."
        ),
        Err(e) => {
            let message = format!("{e:#}");
            assert!(
                message.contains("failed to open pcap capture"),
                "the refusal must say the capture handle could not be opened, got: {message}"
            );
            assert!(
                message.contains("/dev/bpf") || message.contains("CAP_NET_RAW"),
                "the refusal must tell the user which privilege is missing, got: {message}"
            );
        }
    }
}
