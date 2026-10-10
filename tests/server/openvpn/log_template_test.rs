//! The INFO lines OpenVPN's two events render name the peer they came from.
//!
//! `call_llm` builds each line through `EventLogContext::log_complete`, with the client address
//! looked up from a connection registered in AppState. `openvpn_peer_reset` is raised before
//! any is registered (the peer is added only once `accept_peer` answers), so `client_ip` is
//! never filled for it. Both templates take the sender from the event's own `peer_addr`.
//! Model-free: no server, no LLM call.

use netget::llm::actions::protocol_trait::ActionResult;
use netget::protocol::event_logger::EventLogContext;
use netget::protocol::Event;
use netget::server::connection::ConnectionId;
use netget::server::openvpn::actions::{OPENVPN_KEY_EXCHANGE_EVENT, OPENVPN_PEER_RESET_EVENT};
use netget::state::ServerId;
use serde_json::json;

const PEER: &str = "198.51.100.23:41194";

/// Drive `log_complete` the way `call_llm` does when the connection id it was given has no
/// registered connection (so no client address), and return every line sent to the status
/// channel.
fn rendered_lines(event: &Event) -> Vec<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = EventLogContext::new(
        event,
        ServerId::new(1),
        Some(ConnectionId::new(7)),
        None,
        "OpenVPN",
    );
    let no_results: [ActionResult; 0] = [];
    ctx.log_complete(Some(&tx), &no_results);
    drop(tx);
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    lines
}

fn line_at<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} line was rendered: {lines:?}"))
}

#[test]
fn the_peer_reset_info_line_names_the_peer_before_it_is_registered() {
    // The shape `OpenvpnServer::decide_and_answer` builds.
    let event = Event::new(
        &OPENVPN_PEER_RESET_EVENT,
        json!({
            "peer_addr": PEER,
            "client_session_id": "0011223344556677",
            "key_id": 0,
            "reset_type": "ControlHardResetClientV2",
            "packet_id": 0,
            "peer_count": 1,
        }),
    );
    let lines = rendered_lines(&event);

    let info = line_at(&lines, "[INFO] ");
    assert!(
        info.contains(PEER),
        "the INFO line must name the sender ({PEER}): {info:?}"
    );
    assert!(
        info.contains("0011223344556677"),
        "the INFO line must name the session: {info:?}"
    );
    let debug = line_at(&lines, "[DEBUG] ");
    assert!(
        debug.contains(PEER),
        "the DEBUG line must name the sender ({PEER}): {debug:?}"
    );
}

#[test]
fn the_key_exchange_info_line_names_the_peer_from_the_event() {
    // The shape `OpenvpnServer::decide_key_exchange` builds.
    let event = Event::new(
        &OPENVPN_KEY_EXCHANGE_EVENT,
        json!({
            "peer_addr": PEER,
            "username": "alice",
            "password": "hunter2",
            "has_credentials": true,
            "options": "V4,dev-type tun,link-mtu 1559",
            "peer_info": {"IV_VER": "2.6.12"},
        }),
    );
    let lines = rendered_lines(&event);

    let info = line_at(&lines, "[INFO] ");
    assert!(
        info.contains(PEER),
        "the INFO line must name the sender ({PEER}): {info:?}"
    );
    assert!(
        !info.contains("hunter2"),
        "the INFO line must not carry the password: {info:?}"
    );
    let debug = line_at(&lines, "[DEBUG] ");
    assert!(
        debug.contains(PEER),
        "the DEBUG line must name the sender ({PEER}): {debug:?}"
    );
}
