//! Regression tests for the privilege model.
//!
//! Two defects are pinned here:
//!
//! * **Raw IP sockets vs. layer-2 capture were one flag.** `has_raw_socket_access`
//!   covered both, and the probe granted it if *either* succeeded. A macOS user in
//!   the ChmodBPF group — who owns `/dev/bpf*` but cannot open `SOCK_RAW` — was
//!   therefore allowed to start an ICMP server that could only fail with `EPERM`.
//!
//! * **There was no way to express device access.** Bluetooth, USB and NFC need an
//!   adapter or a reader, which is neither a port nor a socket. Every BLE protocol
//!   sat at `None` (a lie that admits everyone) because `Root` would have been a
//!   different lie that refuses users who do have adapter access.
//!
//! No privileges of any kind are needed to run these: the capability set is built
//! by hand so each requirement can be tested against a host that has exactly one
//! thing.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --test privilege_requirement_test -- --test-threads=100

use netget::privilege::{DeviceClass, SystemCapabilities};
use netget::protocol::metadata::{PrivilegeRequirement, PrivilegeSeverity};

/// A host with no privileges and no devices at all.
fn bare() -> SystemCapabilities {
    SystemCapabilities {
        can_bind_privileged_ports: false,
        has_raw_socket_access: false,
        has_packet_capture_access: false,
        has_bluetooth_access: false,
        has_usb_access: false,
        has_nfc_access: false,
        is_root: false,
    }
}

#[test]
fn capture_access_alone_does_not_satisfy_raw_ip_sockets() {
    // The ChmodBPF macOS user: /dev/bpf* readable, no SOCK_RAW, not root.
    let caps = SystemCapabilities {
        has_packet_capture_access: true,
        ..bare()
    };

    assert!(
        PrivilegeRequirement::PacketCapture.is_met_by(&caps),
        "ARP/DataLink/IS-IS must be allowed for a capture-only user"
    );
    assert!(
        !PrivilegeRequirement::RawSockets.is_met_by(&caps),
        "ICMP/IGMP/OSPF must be REFUSED for a capture-only user - this is the case \
         a single has_raw_socket_access flag could not express"
    );
}

/// The protocols named above must actually *declare* what the test above asserts about them.
///
/// The test above names ARP/DataLink/IS-IS but only exercises the enum variant, so it passed
/// for the entire period in which `PacketCapture` existed and none of those three adopted it —
/// all still declared `RawSockets`, and `server_startup.rs` hard-gates on `is_met_by()`, so a
/// capture-only user was refused all three. Testing the mechanism is not testing the adoption.
#[test]
fn the_capture_protocols_declare_packet_capture() {
    use netget::protocol::server_registry::registry;

    for name in ["ARP", "DataLink", "ISIS"] {
        let Some(protocol) = registry().get(name) else {
            continue; // not compiled in under this feature set
        };
        assert_eq!(
            protocol.metadata().privilege_requirement,
            PrivilegeRequirement::PacketCapture,
            "{name} does layer-2 capture, so it must declare PacketCapture. Declaring \
             RawSockets refuses a macOS ChmodBPF user who can in fact run it"
        );
    }
}

#[test]
fn raw_ip_socket_access_alone_does_not_satisfy_capture() {
    let caps = SystemCapabilities {
        has_raw_socket_access: true,
        ..bare()
    };

    assert!(PrivilegeRequirement::RawSockets.is_met_by(&caps));
    assert!(!PrivilegeRequirement::PacketCapture.is_met_by(&caps));
}

#[test]
fn device_access_is_per_class() {
    let caps = SystemCapabilities {
        has_bluetooth_access: true,
        ..bare()
    };

    assert!(PrivilegeRequirement::DeviceAccess(DeviceClass::Bluetooth).is_met_by(&caps));
    assert!(!PrivilegeRequirement::DeviceAccess(DeviceClass::Usb).is_met_by(&caps));
    assert!(!PrivilegeRequirement::DeviceAccess(DeviceClass::Nfc).is_met_by(&caps));

    assert!(caps.has_device_access(DeviceClass::Bluetooth));
    assert!(!caps.has_device_access(DeviceClass::Nfc));
}

#[test]
fn device_access_is_not_root_and_not_a_socket() {
    // Root with no adapter must still be refused a Bluetooth protocol: being root
    // does not conjure hardware. Conversely a non-root user with an adapter must be
    // allowed - the reason BLE protocols could not honestly declare `Root`.
    let rooted_without_adapter = SystemCapabilities {
        is_root: true,
        can_bind_privileged_ports: true,
        has_raw_socket_access: true,
        has_packet_capture_access: true,
        ..bare()
    };
    assert!(!PrivilegeRequirement::DeviceAccess(DeviceClass::Bluetooth)
        .is_met_by(&rooted_without_adapter));

    let user_with_adapter = SystemCapabilities {
        has_bluetooth_access: true,
        ..bare()
    };
    assert!(
        PrivilegeRequirement::DeviceAccess(DeviceClass::Bluetooth).is_met_by(&user_with_adapter)
    );
    assert!(!PrivilegeRequirement::Root.is_met_by(&user_with_adapter));
}

#[test]
fn none_is_met_everywhere_and_root_only_by_root() {
    let caps = bare();
    assert!(PrivilegeRequirement::None.is_met_by(&caps));
    assert!(!PrivilegeRequirement::Root.is_met_by(&caps));

    let root = SystemCapabilities {
        is_root: true,
        ..bare()
    };
    assert!(PrivilegeRequirement::Root.is_met_by(&root));
}

#[test]
fn every_requirement_describes_itself_and_has_a_severity() {
    let all = [
        PrivilegeRequirement::None,
        PrivilegeRequirement::PrivilegedPort(80),
        PrivilegeRequirement::RawSockets,
        PrivilegeRequirement::PacketCapture,
        PrivilegeRequirement::DeviceAccess(DeviceClass::Bluetooth),
        PrivilegeRequirement::DeviceAccess(DeviceClass::Usb),
        PrivilegeRequirement::DeviceAccess(DeviceClass::Nfc),
        PrivilegeRequirement::Root,
    ];

    for req in &all {
        assert!(
            !req.description().is_empty(),
            "{:?} has no description",
            req
        );
    }

    assert_eq!(
        PrivilegeRequirement::None.severity(),
        PrivilegeSeverity::None
    );
    assert_eq!(
        PrivilegeRequirement::Root.severity(),
        PrivilegeSeverity::Root
    );
    assert_eq!(
        PrivilegeRequirement::PacketCapture.severity(),
        PrivilegeSeverity::Elevated,
        "capture is grantable without root (ChmodBPF, CAP_NET_RAW), so it must not \
         be coloured as a root requirement"
    );
    assert_eq!(
        PrivilegeRequirement::DeviceAccess(DeviceClass::Usb).severity(),
        PrivilegeSeverity::Elevated
    );

    // Raw sockets and capture must not describe themselves identically, or the
    // refusal message cannot tell a user which capability they are missing.
    assert_ne!(
        PrivilegeRequirement::RawSockets.description(),
        PrivilegeRequirement::PacketCapture.description()
    );
}

#[test]
fn detected_capabilities_are_self_consistent() {
    let caps = SystemCapabilities::detect();

    if caps.is_root {
        assert!(caps.has_raw_socket_access, "root implies raw IP sockets");
        assert!(caps.has_packet_capture_access, "root implies capture");
        assert!(caps.can_bind_privileged_ports, "root implies port binding");
    }

    // The description must name both network capabilities separately, otherwise a
    // refusal message cannot explain which one the user lacks.
    let desc = caps.description();
    assert!(desc.contains("raw IP sockets"), "description: {}", desc);
    assert!(desc.contains("packet capture"), "description: {}", desc);
}

/// Capture detection must not depend on how busy the machine is.
///
/// A BPF device is **exclusive** — one open handle each — and the probe used to try only
/// `/dev/bpf0..3`. Running the suite at `--test-threads=100`, or alongside another netget,
/// occupies those, and the probe then answered "no capture access" on a host that plainly
/// had it. `PrivilegeRequirement::is_met_by()` gates on that answer, so ARP, DataLink and
/// IS-IS became un-startable whenever the machine was busy — a refusal caused by load, not
/// by permissions.
///
/// It surfaced as `isis_spawn_outcome_matches_capture_privilege` failing with "spawn returned
/// Ok without capture privilege": the capture had opened fine and only the probe disagreed.
///
/// This holds the first handful of BPF devices open and asserts the answer does not change.
/// On a host with no capture access at all both answers are `false`, which is still the
/// property under test: the probe must be *stable*, not permissive.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
#[test]
fn capture_detection_survives_the_first_bpf_devices_being_busy() {
    let before = SystemCapabilities::detect().has_packet_capture_access;

    // Hold whatever we can of the low-numbered devices, exactly the contention the old
    // probe mistook for a permission failure.
    let mut held = Vec::new();
    for n in 0..6 {
        if let Ok(f) = std::fs::File::open(format!("/dev/bpf{n}")) {
            held.push(f);
        }
    }

    let during = SystemCapabilities::detect().has_packet_capture_access;
    drop(held);

    assert_eq!(
        before, during,
        "capture detection changed while the low-numbered /dev/bpf* devices were held. \
         A BPF device is exclusive, so 'busy' is not 'denied' — scan further up, and treat \
         EBUSY as evidence that access exists."
    );
}
