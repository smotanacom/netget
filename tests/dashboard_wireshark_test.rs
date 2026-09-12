//! `[ view in wireshark ]`: the capture recipe the dashboard derives for an
//! instance, and where it is offered (a row on every instance, a button on the
//! create/edit form so the capture can run before the instance exists).

#![cfg(feature = "tcp")]

use netget::tui::app::Section;
use netget::tui::hit::ModalAction;
use netget::tui::inspector::{self, InspectorTab, InstanceAction};
use netget::tui::modal::form::{FieldTarget, FormModel};
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
    // `nfc` and the USB *servers* are deliberately absent: both speak a real protocol over a
    // real socket. See the tests below — this list is for protocols where nothing NetGet runs
    // touches one.
    for name in ["bluetooth_ble_heart_rate", "pty", "stdio"] {
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
}

#[test]
fn the_button_is_a_wireshark_action_on_servers_and_clients() {
    use netget::state::client::ClientStatus;
    use netget::state::server::ServerStatus;
    use netget::state::{ClientId, ServerId};
    use netget::tui::app::{InspectorUi, InstanceRef};
    use netget::tui::projection::{ClientRow, SendState, ServerRow};

    let ui = InspectorUi::default();
    let has_wireshark = |view: &inspector::InspectorView| {
        view.bar
            .iter()
            .any(|b| b.action == InstanceAction::Wireshark && b.label.contains("wireshark"))
    };

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
        uptime_secs: 0,
        client_counterpart: None,
        intercepts: Vec::new(),
    };
    // The overview bar carries it, and so does the config tab's.
    let view = inspector::build(InstanceRef::Server(&server), &ui, None, 60);
    assert_eq!(view.tab, InspectorTab::Overview);
    assert!(has_wireshark(&view));

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
        uptime_secs: 0,
        send_state: SendState::Ready,
        send_actions: Vec::new(),
        intercepts: Vec::new(),
    };
    let view = inspector::build(InstanceRef::Client(&client), &ui, None, 60);
    assert!(has_wireshark(&view));
}

/// The six USB servers are plain TCP listeners speaking USB/IP, and Wireshark has dissected it
/// since 2.4. They used to inherit the nusb *client's* off-network answer because the table
/// matched on the `usb` prefix alone — so the modal told the operator a capture was possible
/// and handed over nothing to run. usbmon is not the alternative: no kernel enumerates these
/// devices, so it cannot see them at all.
#[test]
fn usb_servers_speak_usbip_over_tcp_and_get_a_real_command() {
    for name in ["USB-Keyboard", "usb_msc", "usb-fido2", "usb_smartcard"] {
        let plan = CapturePlan::build(server(name, "127.0.0.1", 3240), Platform::Linux);

        assert_eq!(plan.wire.transport, Transport::Tcp, "{name}");
        assert_eq!(plan.capture_filter, "tcp port 3240", "{name}");
        assert_eq!(
            plan.decode_as.as_deref(),
            Some("tcp.port==3240,usbip"),
            "{name}"
        );
    }
}

/// The bare `usb` protocol is the nusb client, which reaches a real device through the OS.
/// That one genuinely has nothing on a network.
#[test]
fn the_bare_usb_client_stays_off_network() {
    let plan = CapturePlan::build(
        CaptureTarget::client("usb", Some("127.0.0.1:3240")),
        Platform::Linux,
    );
    assert_eq!(plan.wire.transport, Transport::NotNetwork);
    assert_eq!(plan.wireshark_command(), None);
}

/// The redundancy family rides no port, so the entry is a BPF plus a display filter and never
/// a decode-as — tshark already maps ip.proto 112, llc.dsap 0x42, ethertype 0x88cc,
/// llc.cisco_pid 0x2000 and udp.port 1985 to these dissectors by default.
#[test]
fn the_redundancy_family_filters_at_the_link_layer_with_no_decode_as() {
    for (name, filter, display) in [
        ("vrrp", "ip proto 112", "vrrp"),
        ("hsrp", "udp port 1985", "hsrp"),
        ("stp", "ether dst 01:80:c2:00:00:00", "stp"),
        ("lldp", "ether proto 0x88cc", "lldp"),
        ("cdp", "ether dst 01:00:0c:cc:cc:cc", "cdp"),
    ] {
        let plan = CapturePlan::build(server(name, "", 0), Platform::Linux);

        assert_eq!(plan.capture_filter, filter, "{name}");
        assert_eq!(plan.display_filter, display, "{name}");
        assert_eq!(plan.decode_as, None, "{name} needs no decode-as clause");
    }
}

/// CARP shares IP protocol 112 with VRRP and is the default dissector for nothing, so without
/// an explicit clause a CARP packet is dissected as VRRP — and a healthy CARP host then reads
/// as a VRRPv2 master resigning. The note has to say so, because `wire_for` cannot see which
/// variant the server was started with.
#[test]
fn the_vrrp_entry_warns_that_carp_needs_its_own_decode_as() {
    let plan = CapturePlan::build(server("vrrp", "", 0), Platform::Linux);

    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("carp") && n.contains("ip.proto==112")),
        "expected the CARP clause in the notes: {:?}",
        plan.notes
    );
}

/// The three link-layer filters name an Ethernet address or EtherType, which BPF rejects on a
/// DLT_NULL loopback device. That is the `isis`/`arp` trap, and an operator who is not told
/// gets "expression rejects all packets" with no idea why.
#[test]
fn the_ethernet_only_filters_say_they_will_not_work_on_loopback() {
    for name in ["stp", "lldp", "cdp"] {
        let plan = CapturePlan::build(server(name, "", 0), Platform::Linux);
        assert!(
            plan.notes.iter().any(|n| n.contains("loopback")),
            "{name} must warn about the loopback rejection: {:?}",
            plan.notes
        );
    }
}
