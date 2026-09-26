//! Dual-protocol detection (`src/protocol/dual.rs`): the deterministic join
//! between the server and client canonical name tables.

use netget::protocol::dual::{
    alias_table_problems, all_dual_protocols, client_protocol_for_server,
    compiled_client_protocol_for_server,
};

/// The alias table must stay consistent with both registries: every alias side
/// must name a real protocol, and no alias may duplicate what normalization
/// already achieves.
#[test]
fn alias_table_is_consistent() {
    let problems = alias_table_problems();
    assert!(
        problems.is_empty(),
        "alias table problems:\n{}",
        problems.join("\n")
    );
}

/// Golden set: protocols that unambiguously exist on both sides must map.
#[test]
fn golden_duals_map() {
    for server in [
        "TCP",
        "UDP",
        "Telnet",
        "HTTP",
        "DNS",
        "Redis",
        "IRC",
        "FTP",
        "SMTP",
        "IMAP",
        "POP3",
        "MQTT",
        "SSH",
        "WebSocket",
        "PostgreSQL",
        "MySQL",
        "LDAP",
        "NNTP",
        "SNMP",
        "Syslog",
        "TLS",
        "VNC",
        "XMPP",
        "WHOIS",
        "NTP",
        "DHCP",
        "BOOTP",
        "STUN",
    ] {
        assert!(
            client_protocol_for_server(server).is_some(),
            "expected a client counterpart for server protocol {server:?}"
        );
    }
}

/// Divergent-name pairs resolve through the alias table.
#[test]
fn aliased_duals_map() {
    assert_eq!(client_protocol_for_server("DoH"), Some("DNS-over-HTTPS"));
    assert_eq!(client_protocol_for_server("Proxy"), Some("HTTP Proxy"));
    assert_eq!(client_protocol_for_server("Tor Relay"), Some("Tor"));
    assert_eq!(client_protocol_for_server("Bitcoin P2P"), Some("Bitcoin"));
    assert_eq!(client_protocol_for_server("SamlIdp"), Some("SAML"));
    assert_eq!(client_protocol_for_server("SamlSp"), Some("SAML"));
    assert_eq!(client_protocol_for_server("OpenID"), Some("OpenIDConnect"));
    assert_eq!(
        client_protocol_for_server("Torrent-Tracker"),
        Some("BitTorrent Tracker")
    );
    assert_eq!(
        client_protocol_for_server("Torrent-DHT"),
        Some("BitTorrent DHT")
    );
    assert_eq!(
        client_protocol_for_server("Torrent-Peer"),
        Some("BitTorrent Peer Wire")
    );
}

/// Case/punctuation-only differences resolve through normalization, without
/// needing alias entries.
#[test]
fn normalized_duals_map() {
    assert_eq!(client_protocol_for_server("IGMP"), Some("igmp"));
    assert_eq!(client_protocol_for_server("ISIS"), Some("IS-IS"));
    assert_eq!(client_protocol_for_server("KAFKA"), Some("Kafka"));
    assert_eq!(client_protocol_for_server("WireGuard"), Some("wireguard"));
    assert_eq!(client_protocol_for_server("OSPF"), Some("ospf"));
    assert_eq!(
        client_protocol_for_server("SOCKET_FILE"),
        Some("SocketFile")
    );
    assert_eq!(client_protocol_for_server("SSH Agent"), Some("SSH Agent"));
    assert_eq!(
        client_protocol_for_server("BLUETOOTH_BLE"),
        Some("Bluetooth (BLE)")
    );
    // Server protocols that gained a client of the same name.
    assert_eq!(client_protocol_for_server("RADIUS"), Some("RADIUS"));
    assert_eq!(client_protocol_for_server("Modbus"), Some("Modbus"));
    assert_eq!(client_protocol_for_server("CoAP"), Some("CoAP"));
    assert_eq!(client_protocol_for_server("Memcached"), Some("Memcached"));
}

/// Server-only protocols must return None — a false positive here would make
/// the UI offer a client that does not exist.
#[test]
fn server_only_protocols_have_no_dual() {
    for server in [
        "RDP",
        "TFTP",
        "SVN",
        "QUIC",
        "Mercurial",
        "Reverse Shell",
        "OpenVPN",
        "RTSP",
        "HLS",
        "RTP",
        // Profile servers deliberately not paired with the generic base clients:
        "USB-Keyboard",
        "BLUETOOTH_BLE_KEYBOARD",
    ] {
        assert_eq!(
            client_protocol_for_server(server),
            None,
            "server protocol {server:?} unexpectedly mapped to a client"
        );
    }
}

/// Determinism: repeated evaluation yields identical results (guards against
/// any future reintroduction of unordered-map matching).
#[test]
fn mapping_is_deterministic() {
    assert_eq!(all_dual_protocols(), all_dual_protocols());
}

/// `compiled_` only reports clients present in this build's registry, and
/// whatever it reports must agree with the codebase-wide mapping.
#[test]
fn compiled_mapping_is_subset_of_codebase_mapping() {
    for (server, client) in all_dual_protocols() {
        if let Some(compiled) = compiled_client_protocol_for_server(server) {
            assert_eq!(
                compiled, client,
                "compiled mapping disagrees for {server:?}"
            );
        }
    }
    // TCP is in every default/test build; the demo pair must be live.
    #[cfg(feature = "tcp")]
    assert_eq!(
        compiled_client_protocol_for_server("TCP").as_deref(),
        Some("TCP")
    );
}
