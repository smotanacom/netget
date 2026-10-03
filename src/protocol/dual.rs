//! Dual-protocol detection: which server protocols have a client counterpart.
//!
//! The server and client registries use divergent canonical names ("DoH" vs
//! "DNS-over-HTTPS", "Tor Relay" vs "Tor", …), and their keyword matchers are
//! not suitable for joining them (`ClientRegistry::parse_from_str` does greedy
//! substring matching over an unordered map, so an ambiguous input is
//! nondeterministic). This module joins the two canonical name tables
//! deterministically: an explicit alias table first, then normalized-name
//! equality (lowercase, all non-alphanumerics stripped), which absorbs the
//! case- and punctuation-only differences (IGMP/igmp, ISIS/IS-IS,
//! SOCKET_FILE/SocketFile, BLUETOOTH_BLE/"Bluetooth (BLE)", …).
//!
//! `tests/dual_protocol_test.rs` enforces alias-table completeness by diffing
//! both tables through [`alias_table_problems`].

use super::client_registry::{ALL_KNOWN_CLIENT_PROTOCOLS, CLIENT_REGISTRY};
use super::server_registry::ALL_KNOWN_PROTOCOLS;

/// Server-canonical → client-canonical, for names that differ beyond
/// case/punctuation. Both sides must exist in their respective `ALL_KNOWN_*`
/// tables; `alias_table_problems` (and the test built on it) fails otherwise.
///
/// The USB-* and BLUETOOTH_BLE_* profile servers are deliberately absent: the
/// generic "USB" / "Bluetooth (BLE)" clients speak the base transport, not a
/// specific profile, so pairing a profile server with them would promise an
/// exchange the client cannot hold up. Bitcoin P2P is likewise absent: the Bitcoin
/// client speaks Bitcoin Core JSON-RPC over HTTP, not the peer-to-peer wire protocol.
const SERVER_TO_CLIENT_ALIASES: &[(&str, &str)] = &[
    ("DoH", "DNS-over-HTTPS"),
    ("Proxy", "HTTP Proxy"),
    ("Tor Relay", "Tor"),
    ("OpenID", "OpenIDConnect"),
    ("SamlIdp", "SAML"),
    ("SamlSp", "SAML"),
    ("WebRTC Signaling", "WebRTC"),
    ("Torrent-Tracker", "BitTorrent Tracker"),
    ("Torrent-DHT", "BitTorrent DHT"),
    ("Torrent-Peer", "BitTorrent Peer Wire"),
];

/// Lowercase with every non-alphanumeric character stripped, so that
/// "SSH Agent" == "ssh-agent" == "SshAgent".
fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The client-registry canonical name for a server protocol, if any client
/// implementation exists in the codebase (regardless of whether it is compiled
/// into this build). Deterministic; never consults keyword matching.
pub fn client_protocol_for_server(server_name: &str) -> Option<&'static str> {
    if let Some((_, client)) = SERVER_TO_CLIENT_ALIASES
        .iter()
        .find(|(server, _)| server.eq_ignore_ascii_case(server_name))
    {
        return Some(client);
    }
    let wanted = normalize(server_name);
    ALL_KNOWN_CLIENT_PROTOCOLS
        .iter()
        .map(|(name, _feature)| *name)
        .find(|name| normalize(name) == wanted)
}

/// Like [`client_protocol_for_server`], but `Some` only when the client
/// protocol is actually compiled into this build (present in the runtime
/// client registry), so the UI can offer "open a client against this server"
/// truthfully.
pub fn compiled_client_protocol_for_server(server_name: &str) -> Option<String> {
    // A known canonical name keeps its identity even when its server is not compiled.
    // The keyword resolver deliberately falls back from an unavailable FTP to TCP;
    // that CLI convenience must not change which client a protocol is paired with.
    let wanted = normalize(server_name);
    let known_canonical = ALL_KNOWN_PROTOCOLS
        .iter()
        .map(|(name, _)| *name)
        .find(|name| normalize(name) == wanted);
    // Instances preserve the operator's spelling (e.g. `bitcoin`), which may be a
    // registry keyword rather than the canonical server name (`Bitcoin P2P`). Resolve
    // those aliases before joining: the Bitcoin keyword would match the RPC client.
    let canonical = known_canonical.or_else(|| {
        super::server_registry::registry()
            .resolve(server_name)
            .ok()
            .map(|server| server.protocol_name())
    });
    let client_name = client_protocol_for_server(canonical.unwrap_or(server_name))?;
    // The join already returned the client's canonical name. Keyword matching here
    // could substitute another compiled client for an unavailable implementation.
    CLIENT_REGISTRY
        .get(client_name)
        .map(|_| client_name.to_string())
}

/// Every (server canonical name, client canonical name) pair derivable from
/// the two registry tables. Codebase-wide, not restricted to this build.
pub fn all_dual_protocols() -> Vec<(&'static str, &'static str)> {
    ALL_KNOWN_PROTOCOLS
        .iter()
        .filter_map(|(server, _feature)| {
            client_protocol_for_server(server).map(|client| (*server, client))
        })
        .collect()
}

/// Consistency problems in the alias table, for the completeness test:
/// an alias whose server side is not a known server protocol, or whose client
/// side is not a known client protocol, or that duplicates what normalization
/// already achieves (dead weight that will rot).
pub fn alias_table_problems() -> Vec<String> {
    let mut problems = Vec::new();
    for (server, client) in SERVER_TO_CLIENT_ALIASES {
        if !ALL_KNOWN_PROTOCOLS.iter().any(|(name, _)| name == server) {
            problems.push(format!(
                "alias server side {server:?} is not in ALL_KNOWN_PROTOCOLS"
            ));
        }
        if !ALL_KNOWN_CLIENT_PROTOCOLS
            .iter()
            .any(|(name, _)| name == client)
        {
            problems.push(format!(
                "alias client side {client:?} is not in ALL_KNOWN_CLIENT_PROTOCOLS"
            ));
        }
        if normalize(server) == normalize(client) {
            problems.push(format!(
                "alias {server:?} -> {client:?} is redundant: normalization already matches them"
            ));
        }
    }
    problems
}
