//! `[ view in wireshark ]`: the capture recipe the dashboard derives for an
//! instance, and where it is offered (a row on every instance, a button on the
//! create/edit form so the capture can run before the instance exists).

#![cfg(feature = "tcp")]

use netget::tui::app::Section;
use netget::tui::hit::ModalAction;
use netget::tui::modal::form::{FieldTarget, FormModel};
use netget::tui::tree::{self, NodeId, RowAction, TreeState};
use netget::tui::wireshark::{
    wire_for, CapturePlan, CaptureTarget, PlanLine, Platform, Role, Transport,
};

fn server(protocol: &str, host: &str, port: u16) -> CaptureTarget {
    CaptureTarget {
        protocol: protocol.into(),
        role: Role::Server,
        host: Some(host.into()),
        port: Some(port),
        interface: None,
    }
}

#[test]
fn http_server_on_loopback_gets_loopback_interface_port_filters_and_decode_as() {
    let plan = CapturePlan::build(server("HTTP", "127.0.0.1", 8080), Platform::MacOs);
    assert_eq!(plan.interface, "lo0");
    assert_eq!(plan.capture_filter, "tcp port 8080");
    assert_eq!(plan.display_filter, "tcp.port == 8080 && http");
    assert_eq!(plan.decode_as.as_deref(), Some("tcp.port==8080,http"));
    assert_eq!(
        plan.wireshark_command().unwrap(),
        "wireshark -k -i lo0 -f \"tcp port 8080\" -Y \"tcp.port == 8080 && http\" -d tcp.port==8080,http"
    );
    assert!(plan
        .tshark_command()
        .unwrap()
        .starts_with("tshark -l -i lo0 "));
}

#[test]
fn linux_uses_lo_and_any() {
    let local = CapturePlan::build(server("tcp", "127.0.0.1", 9000), Platform::Linux);
    assert_eq!(local.interface, "lo");
    // Plain TCP has no application dissector: no decode-as, filter is the port.
    assert_eq!(local.decode_as, None);
    assert_eq!(local.display_filter, "tcp.port == 9000");

    let everywhere = CapturePlan::build(server("tcp", "0.0.0.0", 9000), Platform::Linux);
    assert_eq!(everywhere.interface, "any");
}

#[test]
fn macos_has_no_any_device_so_a_wildcard_bind_says_so() {
    let plan = CapturePlan::build(server("http", "0.0.0.0", 8080), Platform::MacOs);
    assert_eq!(plan.interface, "lo0");
    assert!(
        plan.notes.iter().any(|n| n.contains("tshark -D")),
        "should tell the user how to find the external interface: {:?}",
        plan.notes
    );
}

#[test]
fn udp_protocols_filter_on_udp() {
    let plan = CapturePlan::build(server("NTP", "127.0.0.1", 1123), Platform::Linux);
    assert_eq!(plan.capture_filter, "udp port 1123");
    assert_eq!(plan.display_filter, "udp.port == 1123 && ntp");
    assert_eq!(plan.decode_as.as_deref(), Some("udp.port==1123,ntp"));
}

#[test]
fn dns_is_served_on_both_transports() {
    let plan = CapturePlan::build(server("DNS", "127.0.0.1", 5353), Platform::Linux);
    assert_eq!(plan.capture_filter, "port 5353");
    assert_eq!(
        plan.display_filter,
        "(tcp.port == 5353 || udp.port == 5353) && dns"
    );
    assert!(plan
        .notes
        .iter()
        .any(|n| n.contains("-d udp.port==5353,dns")));
}

#[test]
fn raw_protocols_have_no_port_and_use_the_declared_interface() {
    let mut target = server("ICMP", "", 0);
    target.interface = Some("en0".into());
    let plan = CapturePlan::build(target, Platform::MacOs);
    assert_eq!(plan.interface, "en0");
    assert_eq!(plan.capture_filter, "icmp");
    assert_eq!(plan.display_filter, "icmp");
    assert_eq!(plan.decode_as, None);
    assert_eq!(wire_for("arp").transport, Transport::Raw("arp"));
    assert_eq!(wire_for("ospf").transport, Transport::Raw("ip proto 89"));
    // `isis` is not a BPF keyword on loopback; the display filter selects it.
    assert_eq!(wire_for("isis").transport, Transport::Raw(""));
    assert_eq!(wire_for("isis").display, Some("isis"));
}

#[test]
fn dissectors_outside_the_port_table_are_named_only_in_the_display_filter() {
    // Verified against tshark: `-d tcp.port==N,drda` and `…,ipp` are rejected.
    let db2 = CapturePlan::build(server("db2", "127.0.0.1", 50000), Platform::Linux);
    assert_eq!(db2.decode_as, None);
    assert_eq!(db2.display_filter, "tcp.port == 50000 && drda");
    let ipp = CapturePlan::build(server("ipp", "127.0.0.1", 631), Platform::Linux);
    assert_eq!(ipp.decode_as.as_deref(), Some("tcp.port==631,http"));
    assert_eq!(ipp.display_filter, "tcp.port == 631 && (ipp || http)");
}

#[test]
fn port_zero_means_unknown_until_started() {
    let plan = CapturePlan::build(server("http", "127.0.0.1", 0), Platform::Linux);
    assert_eq!(plan.capture_filter, "tcp");
    assert_eq!(plan.display_filter, "tcp && http");
    assert_eq!(plan.decode_as, None, "nothing to decode-as without a port");
    assert!(plan.notes.iter().any(|n| n.contains("re-open")));
}

#[test]
fn off_network_protocols_get_an_explanation_instead_of_a_command() {
    // `nfc` is deliberately absent: only its *client* is off-network. See the pair of tests
    // below — this list is for protocols where neither role touches a socket.
    for name in ["USB-Keyboard", "bluetooth_ble_heart_rate", "pty"] {
        let plan = CapturePlan::build(server(name, "", 0), Platform::Linux);
        assert_eq!(plan.wire.transport, Transport::NotNetwork, "{name}");
        assert_eq!(plan.wireshark_command(), None, "{name}");
        assert!(!plan.notes.is_empty(), "{name} must explain itself");
        assert!(
            plan.lines()
                .iter()
                .any(|l| matches!(l, PlanLine::Heading(h) if h == "Notes")),
            "{name}"
        );
    }
}

/// The NFC **client** genuinely cannot be captured: it speaks PC/SC to a physical reader.
#[test]
fn the_nfc_client_explains_that_pc_sc_is_not_on_a_network() {
    let plan = CapturePlan::build(
        CaptureTarget::client("nfc", Some("127.0.0.1:35963")),
        Platform::Linux,
    );

    assert_eq!(plan.wire.transport, Transport::NotNetwork);
    assert_eq!(plan.wireshark_command(), None);
    assert!(
        plan.notes.iter().any(|n| n.contains("usbmon")),
        "the note should point at the one place these APDUs *are* visible: {:?}",
        plan.notes
    );
}

/// The NFC **server** is a plain TCP socket speaking vpcd — no PC/SC call, no reader. Keying
/// the table on the protocol name alone gave both roles the client's answer, so an operator
/// debugging a virtual tag was told to give up on a capture that works.
#[test]
fn the_nfc_server_is_a_tcp_socket_and_gets_a_real_command() {
    let plan = CapturePlan::build(server("nfc", "127.0.0.1", 35963), Platform::Linux);

    assert_eq!(plan.wire.transport, Transport::Tcp);
    assert_eq!(plan.capture_filter, "tcp port 35963");
    assert!(
        plan.wireshark_command().is_some(),
        "the server must hand over a runnable command"
    );
    // There is no vpcd dissector, so the operator has to be told to read the hex pane rather
    // than left wondering why the bytes are undecoded.
    assert!(
        plan.notes.iter().any(|n| n.contains("vpcd")),
        "expected a note about the missing dissector: {:?}",
        plan.notes
    );
}

#[test]
fn client_targets_split_the_remote_address_and_filter_on_the_remote_port() {
    let plan = CapturePlan::build(
        CaptureTarget::client("telnet", Some("127.0.0.1:2323")),
        Platform::MacOs,
    );
    assert_eq!(plan.target.host.as_deref(), Some("127.0.0.1"));
    assert_eq!(plan.target.port, Some(2323));
    assert_eq!(plan.interface, "lo0");
    assert_eq!(plan.capture_filter, "tcp port 2323");
    assert_eq!(plan.decode_as.as_deref(), Some("tcp.port==2323,telnet"));
    assert!(plan.notes.iter().any(|n| n.contains("source port")));

    let v6 = CaptureTarget::client("dns", Some("[::1]:53"));
    assert_eq!(v6.host.as_deref(), Some("::1"));
    assert_eq!(v6.port, Some(53));

    let remote = CapturePlan::build(
        CaptureTarget::client("mqtt", Some("10.1.2.3:1883")),
        Platform::MacOs,
    );
    assert_eq!(remote.capture_filter, "tcp port 1883 and host 10.1.2.3");
    assert!(remote
        .notes
        .iter()
        .any(|n| n.contains("route -n get 10.1.2.3")));
}

#[test]
fn unknown_protocol_names_fall_back_to_plain_tcp() {
    let wire = wire_for("something_new");
    assert_eq!(wire.transport, Transport::Tcp);
    assert_eq!(wire.decode_as, None);
}

#[test]
fn every_instance_offers_the_row_and_the_form_offers_the_button() {
    // The row sits with the lifecycle verbs on both kinds of instance.
    let state = TreeState::default();
    let rows = tree::new_instance_rows();
    assert!(!rows.is_empty());

    let mut form = FormModel::for_create(Section::Servers, "http", Some(8080));
    assert!(form.buttons().contains(&ModalAction::FormWireshark));

    // And the button reads the fields as they are — before Apply, before the
    // server exists — so the capture can be running first.
    form.set_field_value(&FieldTarget::Host, "0.0.0.0".into());
    let target = form.capture_target();
    assert_eq!(target.role, Role::Server);
    assert_eq!(target.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(target.port, Some(8080));

    let mut client = FormModel::for_create(Section::Clients, "telnet", None);
    client.set_field_value(&FieldTarget::RemoteAddr, "127.0.0.1:2323".into());
    let target = client.capture_target();
    assert_eq!(target.role, Role::Client);
    assert_eq!(target.port, Some(2323));
    let _ = state;
}

#[test]
fn the_row_is_a_wireshark_action_on_servers_and_clients() {
    use netget::state::client::ClientStatus;
    use netget::state::server::ServerStatus;
    use netget::state::{ClientId, ServerId};
    use netget::tui::projection::{ClientRow, SendState, ServerRow};

    let server = ServerRow {
        id: ServerId::new(1),
        protocol: "HTTP".into(),
        port: 8080,
        local_addr: Some("127.0.0.1:8080".into()),
        status: ServerStatus::Running,
        instruction: String::new(),
        memory_len: 0,
        startup_params: None,
        routing: None,
        conns: Vec::new(),
        recent: Vec::new(),
        requests: Vec::new(),
        task_count: 0,
        client_counterpart: None,
        intercepts: Vec::new(),
    };
    let rows = tree::server_rows(&server, &TreeState::default());
    assert!(rows.iter().any(
        |r| matches!(r.node, NodeId::Action(_, RowAction::Wireshark))
            && r.label.contains("wireshark")
    ));

    let client = ClientRow {
        id: ClientId::new(1),
        protocol: "telnet".into(),
        remote_addr: "127.0.0.1:2323".into(),
        status: ClientStatus::Connected,
        instruction: String::new(),
        memory_len: 0,
        startup_params: None,
        routing: None,
        connection: None,
        history: Vec::new(),
        requests: Vec::new(),
        task_count: 0,
        send_state: SendState::Ready,
        send_actions: Vec::new(),
        intercepts: Vec::new(),
    };
    let rows = tree::client_rows(&client, &TreeState::default());
    assert!(rows
        .iter()
        .any(|r| matches!(r.node, NodeId::Action(_, RowAction::Wireshark))));
}
