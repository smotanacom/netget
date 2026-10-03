#[path = "helpers/startup_ports.rs"]
mod startup_ports;

use startup_ports::{parse_server_startup, record_server_startup};

#[test]
fn bound_confirmations_merge_by_identity_in_any_order() {
    let mut servers = Vec::new();
    for line in [
        "[SERVER] Server #2 (HTTP) listening on [::1]:43102",
        "[SERVER] Starting server #1 (TCP) on 127.0.0.1:0",
        "Server #2 (HTTP) started, skipping the initial model call.",
        "[SERVER] Server #1 (TCP) listening on 127.0.0.1:43101",
        "[SERVER] Starting server #2 (HTTP) on [::1]:43102",
    ] {
        record_server_startup(&mut servers, parse_server_startup(line).unwrap());
    }
    assert_eq!(servers.len(), 2);
    assert_eq!(
        servers.iter().find(|server| server.id == "1").unwrap().port,
        43101
    );
    assert_eq!(
        servers.iter().find(|server| server.id == "2").unwrap().port,
        43102
    );
    assert!(parse_server_startup("HTTP listening on 127.0.0.1:9999").is_none());
    assert!(parse_server_startup("[SERVER] Server #2 (HTTP) listening on invalid").is_none());
    assert_eq!(
        parse_server_startup("[SERVER] Starting server #3 (stdio) (no listening socket)")
            .unwrap()
            .port,
        0
    );
}

#[test]
fn ephemeral_sockets_remain_owned_while_their_ports_are_used() {
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_addr = first.local_addr().unwrap();
    let second_addr = second.local_addr().unwrap();
    assert_ne!(first_addr.port(), 0);
    assert_ne!(first_addr, second_addr);
    assert!(std::net::TcpListener::bind(first_addr).is_err());
    assert!(std::net::TcpListener::bind(second_addr).is_err());
}
