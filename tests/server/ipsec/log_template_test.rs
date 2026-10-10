//! The INFO line an `ipsec_handshake` event renders names the peer that sent the packet.
//!
//! `call_llm` builds the line through `EventLogContext::log_complete`, with the client address
//! looked up from a registered connection. The honeypot registers none and passes no connection
//! id, so the context has no client address and `client_ip` is never filled. The line has to
//! take the sender from the event's own `peer_addr`. Model-free: no server, no LLM call.

use netget::llm::actions::protocol_trait::ActionResult;
use netget::protocol::event_logger::EventLogContext;
use netget::protocol::Event;
use netget::server::ipsec::actions::IPSEC_HANDSHAKE_EVENT;
use netget::state::ServerId;
use serde_json::json;

/// Event data in the shape `IpsecServer::handle_handshake_initiation` builds it.
fn handshake_event(peer_addr: &str) -> Event {
    Event::new(
        &IPSEC_HANDSHAKE_EVENT,
        json!({
            "peer_addr": peer_addr,
            "packet_size": 256,
            "ike_version": "IKEv2",
            "exchange_type": "IKE_SA_INIT",
            "initiator_spi": "0102030405060708",
            "responder_spi": "0000000000000000",
            "is_initiator": true,
            "is_response": false,
            "message_id": 0,
            "payloads": ["SA", "KE", "NONCE"],
            "honeypot_mode": true,
            "responds_to_peer": false,
        }),
    )
}

/// Drive `log_complete` the way `call_llm` does for this event (no connection, so no client
/// address) and return every line it sent to the status channel.
fn rendered_lines(event: &Event) -> Vec<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = EventLogContext::new(event, ServerId::new(1), None, None, "IPSec");
    let no_results: [ActionResult; 0] = [];
    ctx.log_complete(Some(&tx), &no_results);
    drop(tx);
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

#[test]
fn the_info_line_names_the_peer_without_a_registered_connection() {
    let peer = "203.0.113.7:500";
    let lines = rendered_lines(&handshake_event(peer));

    let info = lines
        .iter()
        .find(|l| l.starts_with("[INFO] "))
        .unwrap_or_else(|| panic!("no INFO line was rendered: {lines:?}"));
    assert!(
        info.contains(peer),
        "the INFO line must name the sender ({peer}): {info:?}"
    );
    assert!(
        info.contains("IKE_SA_INIT"),
        "the INFO line must name the exchange: {info:?}"
    );

    let debug = lines
        .iter()
        .find(|l| l.starts_with("[DEBUG] "))
        .unwrap_or_else(|| panic!("no DEBUG line was rendered: {lines:?}"));
    assert!(
        debug.contains(peer),
        "the DEBUG line must name the sender ({peer}): {debug:?}"
    );
}
