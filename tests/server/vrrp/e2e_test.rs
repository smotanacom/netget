//! VRRP / CARP end-to-end: a real advertisement in, a decision, a real advertisement out —
//! unprivileged.
//!
//! # Why this does not go through the harness
//!
//! Every other server suite starts the `netget` binary and lets `server_startup` bring the
//! protocol up. That cannot work here, and the reason is worth knowing before you try:
//! **`server_startup`'s privilege gate is per-protocol, not per-transport.** VRRP declares
//! `PrivilegeRequirement::RawSockets`, and `requires_privileges` is `!privilege_met` for that
//! variant, so an unprivileged `start_server` is refused *before* the startup parameters are
//! read — including `transport: "udp"`, which needs no privilege at all. Declaring anything
//! weaker would be a lie about the raw transport, which is the real one.
//!
//! So these tests call `Server::spawn(ctx)` directly with a hand-built `SpawnContext`. That
//! still exercises everything this protocol owns: startup-parameter parsing, the bind, the
//! decode, the event, the handler/LLM dispatch, action execution and the re-encode. Only
//! `server_startup`'s own gate is bypassed, and that gate is not this protocol's code.
//!
//! # What the UDP transport is
//!
//! One complete VRRP (or CARP) message per datagram — the same octets the raw transport would
//! put on the wire, with the IP layer simulated. The inbound packets below are the same
//! literals `codec_test.rs` checks against the specification, so what crosses the socket here
//! is independently pinned bytes rather than whatever the encoder happens to produce.
//!
//! Because VRRPv3 folds the IP source and destination into its checksum, the UDP transport
//! reconstructs that pseudo-header by convention: the datagram's sender for inbound, the
//! bound address for outbound, and the VRRP multicast group `224.0.0.18` as the other half.
//! The inbound literals here are VRRPv2 and CARP, whose checksums cover the message alone, so
//! nothing in this file depends on that convention holding.
//!
//! LLM call budget: 4.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features vrrp \
//!       --test server -- vrrp:: --test-threads=100

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::helpers::mock_builder::MockLlmBuilder;
use crate::helpers::mock_ollama::MockOllamaServer;

use netget::llm::actions::protocol_trait::{Protocol, Server as ServerProtocol};
use netget::llm::ollama_client::OllamaClient;
use netget::protocol::{SpawnContext, StartupParams};
use netget::server::vrrp::actions::VrrpProtocol;
use netget::server::vrrp::codec::{
    self, CarpAdvertisement, PseudoHeader, VrrpAdvertisement, VRRP_MULTICAST_IPV4,
};
use netget::state::app_state::AppState;
use netget::state::server::ServerInstance;
use netget::state::ServerId;

/// The group this server is configured for. Priority 150 is deliberately neither the protocol
/// default (100) nor anything the tests' actions ask for, so an assertion on a value that
/// came from here proves the startup parameter was read rather than that a constant matched.
const LOCAL_PRIORITY: u16 = 150;
const LOCAL_VRID: u8 = 1;
const LOCAL_INTERVAL_SECONDS: f64 = 3.0;
const LOCAL_ADDRESSES: [&str; 2] = ["192.168.1.1", "192.168.1.2"];

/// A VRRPv2 advertisement, priority 100 — the same literal `codec_test.rs` pins against
/// RFC 3768 §5.1.
const VRRP_V2_ADVERTISEMENT: [u8; 20] = [
    0x21, 0x01, 0x64, 0x01, 0x00, 0x01, 0xb9, 0x52, 0xc0, 0xa8, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// The same group resigning: priority 0 (RFC 3768 §6.4.3).
const VRRP_V2_RESIGNATION: [u8; 20] = [
    0x21, 0x01, 0x00, 0x01, 0x00, 0x01, 0x1d, 0x53, 0xc0, 0xa8, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// A CARP advertisement (OpenBSD `struct carp_header`), vhid 1, advskew 0, advbase 1.
const CARP_ADVERTISEMENT: [u8; 36] = [
    0x21, 0x01, 0x00, 0x07, 0x00, 0x01, 0xde, 0xf6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

struct Harness {
    /// Kept alive for the duration of the test: dropping the state drops the server task.
    _state: Arc<AppState>,
    addr: SocketAddr,
    status_rx: UnboundedReceiver<String>,
}

fn vrrp_startup_params() -> serde_json::Value {
    serde_json::json!({
        "transport": "udp",
        "variant": "vrrp",
        "version": 3,
        "vrid": LOCAL_VRID,
        "priority": LOCAL_PRIORITY,
        "advert_interval": LOCAL_INTERVAL_SECONDS as i64,
        "addresses": LOCAL_ADDRESSES,
        "advskew": 0,
        "carp_passphrase": "",
    })
}

/// Bring a VRRP/CARP server up on the UDP transport, pointed at `ollama_url`.
///
/// `ollama_url` is the only knob the silence test needs: pointing it at a closed port is how
/// an LLM failure is produced without a mock that has to be told to misbehave.
#[allow(deprecated)] // SpawnContext::listen_addr is the legacy binding field; still required.
async fn start_vrrp_over_udp(
    ollama_url: String,
    instruction: &str,
    startup_params: serde_json::Value,
) -> Harness {
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
            "VRRP".to_string(),
            instruction.to_string(),
        ))
        .await;

    let protocol = VrrpProtocol::new();
    let params = StartupParams::new(startup_params, protocol.get_startup_parameters())
        .expect("every parameter the test passes must be declared by the protocol");

    let (status_tx, status_rx) = mpsc::unbounded_channel();
    let addr = protocol
        .spawn(SpawnContext {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            mac_address: None,
            interface: None,
            host: Some("127.0.0.1".to_string()),
            port: Some(0),
            llm_client: OllamaClient::new(ollama_url),
            state: state.clone(),
            status_tx,
            server_id,
            startup_params: Some(params),
        })
        .await
        .expect("VRRP must start on the UDP transport without privilege");
    assert_ne!(addr.port(), 0, "spawn must report the bound port");

    Harness {
        _state: state,
        addr,
        status_rx,
    }
}

async fn peer_socket(server: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind peer");
    socket
        .connect(server)
        .await
        .expect("connect to VRRP server");
    socket
}

async fn recv_packet(socket: &UdpSocket, secs: u64) -> Option<Vec<u8>> {
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

/// An advertisement arrives, the model is asked, and what it decides goes back on the wire as
/// a real VRRPv3 advertisement.
///
/// The model is told to take the gateway with priority 200 — the actual hazard this protocol
/// makes possible — and the assertion is that the packet coming back really carries priority
/// 200, and that every field the action did **not** name came from the startup parameters
/// rather than from a constant. That last group is the `ospf` defect the root `CLAUDE.md`
/// records: four of its six parameters were advertised to the model and reached the wire from
/// nowhere.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_advertisement_produces_the_advertisement_the_model_decided_on() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("vrrp_advertisement_received")
            // Deliberately names ONLY the priority. Everything else must come from the
            // server's configured group.
            .respond_with_actions(serde_json::json!([{
                "type": "send_vrrp_advertisement",
                "priority": 200
            }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let harness = start_vrrp_over_udp(
        mock.base_url(),
        "You are a VRRP router on an isolated lab segment. Take the gateway with priority 200.",
        vrrp_startup_params(),
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&VRRP_V2_ADVERTISEMENT)
        .await
        .expect("send advertisement");

    let reply = recv_packet(&peer, 30)
        .await
        .expect("the server must answer with the advertisement the model asked for");

    let advertisement = VrrpAdvertisement::decode(&reply).expect("the reply must be valid VRRP");
    assert_eq!(
        advertisement.priority, 200,
        "the model claimed priority 200 and that is what must be on the wire — this is the \
         field that decides who the segment's default gateway is"
    );

    // Everything the action omitted comes from the startup parameters.
    assert_eq!(
        advertisement.version, 3,
        "the configured version must fill in what the action omitted; note the inbound packet \
         was v2, so this cannot have been copied from it"
    );
    assert_eq!(advertisement.vrid, LOCAL_VRID);
    assert_eq!(
        advertisement.advert_interval_seconds, LOCAL_INTERVAL_SECONDS,
        "the configured advert_interval must reach the wire (the inbound packet said 1s)"
    );
    assert_eq!(
        advertisement
            .addresses
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>(),
        LOCAL_ADDRESSES.to_vec(),
        "the configured virtual addresses must reach the wire (the inbound packet carried one)"
    );
    assert_eq!(
        reply.len(),
        8 + 4 * LOCAL_ADDRESSES.len(),
        "a VRRPv3 advertisement has no trailing authentication data"
    );

    // The reply's checksum must be correct under the RFC 5798 pseudo-header the transport
    // used: our bound address as source, the VRRP group as destination.
    assert!(
        codec::checksum_is_valid(
            &reply,
            Some(&PseudoHeader::new(
                Ipv4Addr::new(127, 0, 0, 1),
                VRRP_MULTICAST_IPV4
            ))
        ),
        "the VRRPv3 checksum must cover the pseudo-header; a peer discards anything else, \
         which looks exactly like the server being down"
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// A priority-0 advertisement raises `vrrp_master_resigned`, not `vrrp_advertisement_received`.
///
/// The two events exist because they are different questions: one is "somebody is master",
/// the other is "the master just stood down and the election is open right now". Routing a
/// resignation to the wrong event would make an operator's handler for it never match, and a
/// resignation is precisely the moment a takeover succeeds.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_priority_zero_advertisement_raises_the_master_resigned_event() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("vrrp_master_resigned")
            .respond_with_actions(serde_json::json!([{
                "type": "send_vrrp_advertisement",
                "priority": 255
            }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let harness = start_vrrp_over_udp(
        mock.base_url(),
        "You are a VRRP router. When the master resigns, claim the group.",
        vrrp_startup_params(),
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&VRRP_V2_RESIGNATION)
        .await
        .expect("send resignation");

    let reply = recv_packet(&peer, 30)
        .await
        .expect("the resignation must reach vrrp_master_resigned and be answered");
    let advertisement = VrrpAdvertisement::decode(&reply).expect("valid VRRP");
    assert_eq!(advertisement.priority, 255);
    assert!(
        advertisement.is_address_owner(),
        "255 is reserved for the router that owns the addresses — the strongest claim there is"
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// `no_advertisement` is a real answer: nothing goes on the wire, and the log says the model
/// chose it.
///
/// This matters because on the wire it is indistinguishable from every other silence. The
/// `decision=model_reject` tag is the only place the difference survives, exactly as `radius`
/// separates a model denial from a fail-closed default.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_advertisement_transmits_nothing_and_is_logged_as_a_decision() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("vrrp_advertisement_received")
            .respond_with_actions(serde_json::json!([{ "type": "no_advertisement" }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let mut harness = start_vrrp_over_udp(
        mock.base_url(),
        "Observe the VRRP group. Do not take part in the election.",
        vrrp_startup_params(),
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&VRRP_V2_ADVERTISEMENT)
        .await
        .expect("send advertisement");

    let decision = wait_for_status(&mut harness.status_rx, "decision=model_reject", 30)
        .await
        .expect(
            "an explicit no_advertisement must be logged as decision=model_reject — without \
             the tag it is indistinguishable from a backend failure",
        );
    assert!(decision.contains("no advertisement"), "got: {decision}");

    assert!(
        recv_packet(&peer, 3).await.is_none(),
        "no_advertisement means no packet. Anything on the wire here is a claim to the \
         gateway address that nobody decided to make."
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
}

/// **The silence test.** When the LLM call fails, nothing goes on the wire.
///
/// VRRP is in the deliberately-silent class and its case is among the strongest in the tree:
/// every message it defines is a positive claim to own the virtual gateway address, and there
/// is no error or NAK message to send instead. A fabricated advertisement that happened to
/// win the election would make every host on the segment route through NetGet, which does not
/// forward — a black hole. The peer already handles our silence correctly: its master-down
/// interval expires and it takes over, which is the spec-defined outcome for a router that
/// stops speaking.
///
/// A bare "no packet arrived" would prove nothing on its own: it is equally consistent with a
/// server that never received the advertisement. So this asserts the pair — the
/// `decision=fail_closed_` line proves the packet *was* decoded, the event *was* raised and
/// the model *was* asked, and the absent packet proves the failure produced no wire output.
/// `an_advertisement_produces_the_advertisement_the_model_decided_on` is the positive control
/// for the same path.
///
/// LLM calls: 0 reach a mock; the backend is a closed port on purpose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_llm_failure_puts_nothing_on_the_wire() {
    // Port 1 is not listening. Nothing else about the server changes.
    let mut harness = start_vrrp_over_udp(
        "http://127.0.0.1:1".to_string(),
        "You are a VRRP router on an isolated lab segment. Take the gateway with priority 200.",
        vrrp_startup_params(),
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&VRRP_V2_ADVERTISEMENT)
        .await
        .expect("send advertisement");

    let decision = wait_for_status(&mut harness.status_rx, "decision=fail_closed_", 90)
        .await
        .expect(
            "the LLM failure must be recorded with a decision= tag. If this times out the \
             event never reached dispatch and the assertion below proves nothing.",
        );
    assert!(
        decision.contains("nothing transmitted"),
        "the operator log must say plainly that no advertisement was sent, got: {decision}"
    );

    assert!(
        recv_packet(&peer, 5).await.is_none(),
        "an LLM failure must put NOTHING on the wire. A fabricated advertisement here asserts \
         gateway ownership netget cannot back and can black-hole the whole segment."
    );
}

// ---------------------------------------------------------------------------
// CARP
// ---------------------------------------------------------------------------

/// CARP goes through the same path and comes out as a CARP packet, not a VRRP one.
///
/// This is the check that the `variant` parameter really switches both directions: the
/// inbound literal is a genuine `struct carp_header` (which a VRRP decoder would misread as a
/// master resigning — see `codec_test.rs`), and the reply must be 36 octets of CARP with the
/// configured vhid, advbase and passphrase-derived HMAC.
///
/// LLM calls: 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_carp_server_decodes_and_answers_in_carp() {
    let mock = MockOllamaServer::start(
        MockLlmBuilder::new()
            .on_event("vrrp_advertisement_received")
            .respond_with_actions(serde_json::json!([{
                "type": "send_vrrp_advertisement",
                "advskew": 10,
                "counter": 5
            }]))
            .expect_calls(1)
            .build(),
    )
    .await
    .expect("mock ollama");

    let harness = start_vrrp_over_udp(
        mock.base_url(),
        "You are a CARP host on an isolated lab segment. Contest the group with a low advskew.",
        serde_json::json!({
            "transport": "udp",
            "variant": "carp",
            "vrid": 3,
            "advert_interval": 2,
            "addresses": ["10.0.0.1"],
            "advskew": 40,
            "carp_passphrase": "lab-secret",
        }),
    )
    .await;
    let peer = peer_socket(harness.addr).await;

    peer.send(&CARP_ADVERTISEMENT)
        .await
        .expect("send CARP advertisement");

    let reply = recv_packet(&peer, 30)
        .await
        .expect("a CARP server must answer a CARP advertisement");
    assert_eq!(reply.len(), 36, "a CARP advertisement is always 36 octets");

    let advertisement = CarpAdvertisement::decode(&reply).expect("the reply must be valid CARP");
    assert_eq!(
        advertisement.vhid, 3,
        "vhid comes from the startup parameter"
    );
    assert_eq!(
        advertisement.advskew, 10,
        "the action's advskew must beat the configured 40 — lower wins in CARP"
    );
    assert_eq!(advertisement.counter, 5);
    assert_eq!(
        advertisement.advbase, 2,
        "advbase comes from the configured advert_interval"
    );
    assert!(
        codec::checksum_is_valid(&reply, None),
        "CARP's checksum covers the message alone; there is no pseudo-header"
    );

    // The passphrase startup parameter really keys the HMAC — with no passphrase the same
    // packet would carry a different authentication field.
    let addresses = [Ipv4Addr::new(10, 0, 0, 1)];
    assert_eq!(
        advertisement.hmac,
        codec::carp_hmac(b"lab-secret", 3, &addresses, 5)
    );
    assert_ne!(
        advertisement.hmac,
        codec::carp_hmac(b"", 3, &addresses, 5),
        "a configured carp_passphrase that does not change the HMAC is a parameter that does \
         nothing"
    );

    mock.wait_for_expectations(30).await;
    mock.verify_calls().await.expect("mock expectations");
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
    let declared: std::collections::BTreeSet<String> = VrrpProtocol::new()
        .get_startup_parameters()
        .into_iter()
        .map(|p| p.name)
        .collect();

    let expected: std::collections::BTreeSet<String> = [
        "transport",
        "variant",
        "version",
        "vrid",
        "priority",
        "advert_interval",
        "addresses",
        "advskew",
        "carp_passphrase",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();

    assert_eq!(
        declared, expected,
        "the declared parameter set drifted from the set VrrpGroupConfig::from_startup_params \
         reads. A declared parameter nothing reads is an advertised knob that does nothing."
    );

    let err = StartupParams::new(
        serde_json::json!({ "prioritee": 0 }),
        VrrpProtocol::new().get_startup_parameters(),
    )
    .expect_err("an undeclared parameter must be refused, not ignored");
    assert!(format!("{err}").contains("prioritee"));
}

/// An interval the configured version cannot encode refuses at **startup**, rather than
/// failing on every advertisement the server later tries to send.
///
/// 60 seconds is perfectly legal under VRRPv2 (a one-octet field of whole seconds) and
/// impossible under VRRPv3, whose 12-bit centisecond field tops out at 40.95 s. That
/// cross-field interaction is exactly the kind a per-parameter range check misses.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(deprecated)]
async fn an_interval_the_configured_version_cannot_encode_refuses_to_start() {
    async fn try_start(params: serde_json::Value) -> anyhow::Result<SocketAddr> {
        let state = Arc::new(AppState::new_with_options(
            false,
            "http://127.0.0.1:1".to_string(),
        ));
        let server_id = state
            .add_server(ServerInstance::new(
                ServerId::new(0),
                0,
                "VRRP".to_string(),
                "router".to_string(),
            ))
            .await;
        let (status_tx, _rx) = mpsc::unbounded_channel();
        let protocol = VrrpProtocol::new();
        let params = StartupParams::new(params, protocol.get_startup_parameters())
            .expect("the keys themselves are declared");

        protocol
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
    }

    let err = try_start(serde_json::json!({
        "transport": "udp", "version": 3, "advert_interval": 60
    }))
    .await
    .expect_err("60 seconds does not fit VRRPv3's 12-bit centisecond field");
    let message = format!("{err:#}");
    assert!(
        message.contains("CENTISECONDS") && message.contains("40.95"),
        "the refusal must explain the unit and the ceiling, got: {message}"
    );

    // The identical interval is fine under v2, which is what makes this a real cross-field
    // check rather than a range check on one parameter.
    try_start(serde_json::json!({
        "transport": "udp", "version": 2, "advert_interval": 60
    }))
    .await
    .expect("60 whole seconds is legal in VRRPv2");

    // A malformed virtual address is also caught at startup, by name.
    let err = try_start(serde_json::json!({
        "transport": "udp", "addresses": ["not-an-address"]
    }))
    .await
    .expect_err("a virtual address that cannot be parsed must refuse the start");
    assert!(format!("{err:#}").contains("not-an-address"));
}

/// `execute_action` runs with no server configuration in scope, so it must not reject a CARP
/// action just because the model did not spell `variant` out.
///
/// A model talking to a CARP server has no reason to name the variant — the server already
/// knows. Rejecting its `advskew` as "a CARP field in a VRRP advertisement" would make the
/// whole CARP path unreachable, which is what happened the first time this was written.
/// Contradictory actions must still be refused.
#[test]
fn execute_action_infers_the_variant_when_the_configuration_is_not_in_scope() {
    let protocol = VrrpProtocol::new();

    protocol
        .execute_action(serde_json::json!({
            "type": "send_vrrp_advertisement", "advskew": 10, "counter": 5
        }))
        .expect("a CARP-shaped action without an explicit variant must be accepted");

    protocol
        .execute_action(serde_json::json!({
            "type": "send_vrrp_advertisement", "priority": 200
        }))
        .expect("a VRRP-shaped action without an explicit variant must be accepted");

    // Both families at once is a real contradiction: VRRP elects on priority (higher wins)
    // and CARP on advskew (lower wins), and there is no packet that carries both.
    let err = protocol
        .execute_action(serde_json::json!({
            "type": "send_vrrp_advertisement", "priority": 200, "advskew": 10
        }))
        .expect_err("priority and advskew together cannot be encoded as anything");
    assert!(format!("{err:#}").contains("advskew"));

    // An explicit variant always wins over the inference.
    let err = protocol
        .execute_action(serde_json::json!({
            "type": "send_vrrp_advertisement", "variant": "carp", "priority": 200
        }))
        .expect_err("CARP has no priority field");
    assert!(
        format!("{err:#}").contains("advskew"),
        "the refusal must point at the field CARP actually elects on"
    );

    assert!(protocol
        .execute_action(serde_json::json!({ "type": "no_advertisement" }))
        .is_ok());
    assert!(protocol
        .execute_action(serde_json::json!({ "type": "send_hsrp_hello" }))
        .is_err());
}

/// The raw transport must never report success without the socket it needs.
///
/// This is the ARP/DataLink/ICMP regression the root `CLAUDE.md` records: a `spawn` that fires
/// the privileged step off into a background task and returns `Ok` before the result is
/// known, leaving a server in `Running` that has received nothing, forever. VRRP creates the
/// raw socket synchronously before anything is spawned; this is what keeps it that way.
///
/// Skipped when the host *does* have raw-socket access, because there the open legitimately
/// succeeds and there is nothing to assert that would hold everywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(deprecated)]
async fn the_raw_transport_never_reports_success_without_a_raw_socket() {
    if netget::privilege::SystemCapabilities::detect().has_raw_socket_access {
        return;
    }

    let state = Arc::new(AppState::new_with_options(
        false,
        "http://127.0.0.1:1".to_string(),
    ));
    let server_id = state
        .add_server(ServerInstance::new(
            ServerId::new(0),
            0,
            "VRRP".to_string(),
            "router".to_string(),
        ))
        .await;
    let (status_tx, _rx) = mpsc::unbounded_channel();
    let protocol = VrrpProtocol::new();
    let params = StartupParams::new(
        serde_json::json!({ "transport": "raw" }),
        protocol.get_startup_parameters(),
    )
    .unwrap();

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
        .expect_err(
            "SOCK_RAW on IP protocol 112 cannot be opened unprivileged, so spawn must return \
             Err rather than a server that sits in Running having received nothing",
        );

    let message = format!("{err:#}");
    assert!(
        message.contains("CAP_NET_RAW") && message.contains("112"),
        "the refusal must name the privilege and the protocol number, got: {message}"
    );
    assert!(
        message.contains("udp"),
        "the refusal must point at the unprivileged transport, got: {message}"
    );
}
