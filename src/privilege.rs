use std::net::{TcpListener, UdpSocket};
use tracing::{debug, warn};

/// A class of local hardware device a protocol needs to open.
///
/// Distinct from every network privilege: opening a Bluetooth adapter or a USB
/// device is neither a port bind nor a socket, and on a correctly configured
/// desktop it needs no elevation at all — a user in the right group, or one who has
/// granted the app permission, simply has it. Declaring `Root` for these would be
/// false *and* would refuse users who do have access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceClass {
    /// A Bluetooth adapter (BlueZ over D-Bus on Linux, CoreBluetooth on macOS)
    Bluetooth,
    /// A USB device (libusb / IOKit)
    Usb,
    /// An NFC reader (PC/SC — pcscd on Linux, the PCSC framework on macOS)
    Nfc,
}

impl DeviceClass {
    /// Short lowercase name used in messages
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bluetooth => "Bluetooth adapter",
            Self::Usb => "USB device",
            Self::Nfc => "NFC reader",
        }
    }

    /// Platform-specific hint for a user who has been refused this device class
    pub fn access_hint(&self) -> &'static str {
        match self {
            Self::Bluetooth => {
                "Linux: install/start BlueZ and ensure a powered adapter exists \
                 (`bluetoothctl list`). macOS: grant Bluetooth permission in \
                 System Settings > Privacy & Security > Bluetooth."
            }
            Self::Usb => {
                "Linux: add a udev rule granting your user access to the device, or \
                 join the group owning /dev/bus/usb. macOS: no elevation is normally \
                 needed."
            }
            Self::Nfc => {
                "Linux: install and start pcscd (`systemctl start pcscd`). macOS: the \
                 PCSC framework is built in."
            }
        }
    }
}

/// System capabilities detected at startup
#[derive(Debug, Clone)]
pub struct SystemCapabilities {
    /// Whether we can bind to privileged ports (< 1024)
    pub can_bind_privileged_ports: bool,
    /// Whether we can open **raw IP sockets** (`SOCK_RAW`) — what ICMP, IGMP and
    /// OSPF need. On Linux this is `CAP_NET_RAW`; elsewhere it is root.
    ///
    /// Deliberately *not* the same thing as [`Self::has_packet_capture_access`]:
    /// this flag used to cover both, and a single flag cannot refuse an ICMP server
    /// for a user who only has capture rights.
    pub has_raw_socket_access: bool,
    /// Whether we can capture/inject at layer 2 via BPF or `AF_PACKET` — what ARP,
    /// DataLink and IS-IS need.
    ///
    /// Genuinely separable from raw IP sockets: a macOS user in the ChmodBPF group
    /// owns `/dev/bpf*` without being root, and so has capture but not raw sockets.
    pub has_packet_capture_access: bool,
    /// Whether a Bluetooth adapter appears to be reachable
    pub has_bluetooth_access: bool,
    /// Whether USB devices appear to be reachable
    pub has_usb_access: bool,
    /// Whether an NFC/PC-SC reader stack appears to be reachable
    pub has_nfc_access: bool,
    /// Whether running as root/administrator
    pub is_root: bool,
}

impl SystemCapabilities {
    /// Detect system capabilities at startup
    pub fn detect() -> Self {
        let is_root = is_running_as_root();
        let can_bind_privileged_ports = can_bind_privileged_port();
        let has_raw_socket_access = has_raw_ip_socket_capability();
        let has_packet_capture_access = has_packet_capture_capability();
        let has_bluetooth_access = has_bluetooth_capability();
        let has_usb_access = has_usb_capability();
        let has_nfc_access = has_nfc_capability();

        debug!(
            "Detected capabilities: root={}, privileged_ports={}, raw_ip_sockets={}, \
             packet_capture={}, bluetooth={}, usb={}, nfc={}",
            is_root,
            can_bind_privileged_ports,
            has_raw_socket_access,
            has_packet_capture_access,
            has_bluetooth_access,
            has_usb_access,
            has_nfc_access
        );

        Self {
            can_bind_privileged_ports,
            has_raw_socket_access,
            has_packet_capture_access,
            has_bluetooth_access,
            has_usb_access,
            has_nfc_access,
            is_root,
        }
    }

    /// Whether the given device class appears to be reachable
    pub fn has_device_access(&self, class: DeviceClass) -> bool {
        match class {
            DeviceClass::Bluetooth => self.has_bluetooth_access,
            DeviceClass::Usb => self.has_usb_access,
            DeviceClass::Nfc => self.has_nfc_access,
        }
    }

    /// Get a human-readable description of capabilities
    pub fn description(&self) -> String {
        let mut parts = Vec::new();

        if self.is_root {
            parts.push("running as root/admin");
        }

        parts.push(if self.can_bind_privileged_ports {
            "privileged ports available"
        } else {
            "privileged ports unavailable"
        });

        parts.push(if self.has_raw_socket_access {
            "raw IP sockets available"
        } else {
            "raw IP sockets unavailable"
        });

        parts.push(if self.has_packet_capture_access {
            "packet capture available"
        } else {
            "packet capture unavailable"
        });

        // Device access is only worth mentioning when something is missing: on a
        // normal desktop all three are present and the line is noise.
        if !self.has_bluetooth_access {
            parts.push("no Bluetooth adapter");
        }
        if !self.has_usb_access {
            parts.push("no USB access");
        }
        if !self.has_nfc_access {
            parts.push("no NFC reader stack");
        }

        parts.join(", ")
    }
}

/// Check if running as root/administrator
fn is_running_as_root() -> bool {
    #[cfg(unix)]
    {
        // geteuid() is the actual answer. The previous implementation stat'd /root
        // and compared $USER/$LOGNAME, which is wrong in both directions: it reported
        // root under `sudo -E` (which preserves $USER) or in any container with a
        // world-readable /root, and reported non-root for a root account whose home
        // is not /root.
        //
        // SAFETY: geteuid() takes no arguments, cannot fail, and touches no memory.
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(windows)]
    {
        // On Windows, check if running as Administrator
        // For now, return false - can be enhanced later with Windows-specific APIs
        false
    }
}

/// What a single privileged-port bind attempt actually proved.
///
/// The distinction exists because a failed bind is not one fact but three, and the
/// original probe collapsed them: it tried 80, 67, 123 and 53, and returned `false` if
/// none bound. A developer machine with anything already listening on those ports —
/// a local web server on 80, `dnsmasq` on 53, an NTP daemon on 123 — was therefore
/// reported as unable to bind privileged ports, and `server_startup` refused to start
/// servers the machine could in fact run. "Someone else holds this port" says nothing
/// whatsoever about this process's privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProbe {
    /// The bind succeeded: proof the process *can* bind privileged ports.
    Bound,
    /// `EACCES`/`EPERM`: proof it *cannot*.
    Denied,
    /// `EADDRINUSE`/`EADDRNOTAVAIL`, or any other error: the port was unavailable for
    /// reasons unrelated to privilege, so this attempt proved nothing either way.
    Inconclusive,
}

/// Classify a failed `bind(2)` by what it says about privilege.
pub fn classify_bind_error(err: &std::io::Error) -> PortProbe {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => PortProbe::Denied,
        // AddrInUse: another process holds it. AddrNotAvailable: the address is not
        // ours to bind. Neither is a statement about privilege.
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::AddrNotAvailable => {
            PortProbe::Inconclusive
        }
        _ => PortProbe::Inconclusive,
    }
}

/// Decide the capability from a set of probe outcomes.
///
/// - Any `Bound` ⇒ capable. One success is proof.
/// - Otherwise any `Denied` ⇒ not capable. The kernel said so explicitly.
/// - Otherwise (every probe inconclusive, e.g. every test port already in use) the
///   honest answer is **unknown**, and we report `true` — capable.
///
/// That last choice is deliberate. This flag *hard-gates* startup in
/// `server_startup.rs`, so mapping "unknown" to `false` turns "we could not tell" into
/// a refusal, which is the failure mode the capability probes were rewritten to
/// eliminate. Reporting `true` on unknown costs nothing worse than the real `bind`
/// failing a moment later with its own `EACCES`, which is a clear, specific, actionable
/// error — strictly better than refusing a machine that would have worked. Note this is
/// a *capability* probe, not an authorization decision: the permissive default here
/// grants no access that the kernel would not independently grant.
pub fn privileged_port_capability(probes: &[PortProbe]) -> bool {
    if probes.contains(&PortProbe::Bound) {
        return true;
    }
    if probes.contains(&PortProbe::Denied) {
        return false;
    }
    true
}

/// Check if we can bind to privileged ports (< 1024)
///
/// Probes several well-known privileged ports and classifies each failure; see
/// [`PortProbe`] and [`privileged_port_capability`] for why the failures must be
/// told apart.
fn can_bind_privileged_port() -> bool {
    // If we're root, we definitely can
    if is_running_as_root() {
        return true;
    }

    // Try to bind to common privileged ports
    // Use a quick test bind that doesn't actually listen
    let test_ports = [80, 67, 123, 53];
    let mut probes = Vec::with_capacity(test_ports.len() * 2);

    for port in test_ports {
        let addr = format!("127.0.0.1:{}", port);

        // Try TCP first
        match TcpListener::bind(&addr) {
            Ok(listener) => {
                drop(listener);
                debug!("Successfully test-bound to privileged TCP port {}", port);
                return true;
            }
            Err(e) => {
                let outcome = classify_bind_error(&e);
                debug!(
                    "TCP bind to privileged port {} failed: {} ({:?})",
                    port, e, outcome
                );
                probes.push(outcome);
            }
        }

        // Try UDP
        match UdpSocket::bind(&addr) {
            Ok(socket) => {
                drop(socket);
                debug!("Successfully test-bound to privileged UDP port {}", port);
                return true;
            }
            Err(e) => {
                let outcome = classify_bind_error(&e);
                debug!(
                    "UDP bind to privileged port {} failed: {} ({:?})",
                    port, e, outcome
                );
                probes.push(outcome);
            }
        }
    }

    // If all test binds failed, check if it's because of privileges or port in use
    // Try binding to a high port to make sure networking works
    if TcpListener::bind("127.0.0.1:0").is_err() {
        warn!("Cannot bind to any ports - networking may be broken");
    }

    let capable = privileged_port_capability(&probes);
    if capable {
        warn!(
            "Every privileged test port ({:?}) was unavailable for reasons other than \
             permission - assuming privileged ports are bindable. A server on a port \
             below 1024 will report the real error if this assumption is wrong.",
            test_ports
        );
    } else {
        debug!("Cannot bind to privileged ports - requires elevated privileges");
    }
    capable
}

/// Check whether this process can open a **raw IP socket** (`SOCK_RAW`).
///
/// This is the capability ICMP, IGMP and OSPF need, and it must be *probed*, not
/// inferred. The original implementation called `pcap::Device::list()`, which is a
/// thin wrapper over `getifaddrs(3)` and succeeds for any unprivileged user — so it
/// reported `true` unconditionally, which in turn made the pre-flight in
/// `server_startup` never fire. Every raw-socket protocol then failed later with an
/// opaque `EPERM` from its own socket call instead of a clear refusal at startup.
///
/// Layer-2 capture is a *separate* capability; see
/// [`has_packet_capture_capability`]. Merging the two — as a single flag once did —
/// makes it impossible to refuse an ICMP server for a capture-only user.
fn has_raw_ip_socket_capability() -> bool {
    #[cfg(unix)]
    {
        if is_running_as_root() {
            return true;
        }

        // A raw IP socket is the definitive test: it succeeds exactly with
        // CAP_NET_RAW on Linux, and only as root on macOS/BSD. Note SOCK_RAW
        // specifically — Linux lets unprivileged users open SOCK_DGRAM ICMP sockets
        // via ping_group_range, which is not the capability these protocols need.
        //
        // SAFETY: socket(2) with constant arguments; the fd is closed immediately
        // on success and nothing else touches it.
        let raw_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP) };
        if raw_fd >= 0 {
            unsafe { libc::close(raw_fd) };
            debug!("Opened a raw ICMP socket - raw IP socket access available");
            return true;
        }

        debug!(
            "Raw ICMP socket failed ({}) and not root - raw-socket protocols \
             (ICMP, IGMP, OSPF) will be refused at startup.",
            std::io::Error::last_os_error()
        );
        false
    }

    #[cfg(windows)]
    {
        is_running_as_root()
    }
}

/// Check whether this process can capture or inject frames at layer 2.
///
/// This is what ARP, DataLink and IS-IS need, and it is genuinely distinct from a
/// raw IP socket: a macOS user in the ChmodBPF group owns `/dev/bpf*` and can
/// capture without being root, while still being unable to open `SOCK_RAW`.
fn has_packet_capture_capability() -> bool {
    #[cfg(unix)]
    {
        if is_running_as_root() {
            return true;
        }

        // Linux: AF_PACKET is the capture path, gated on CAP_NET_RAW.
        //
        // SAFETY: socket(2) with constant arguments; the fd is closed immediately
        // on success and nothing else touches it.
        #[cfg(target_os = "linux")]
        {
            let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, 0) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
                debug!("Opened an AF_PACKET socket - L2 capture available");
                return true;
            }
        }

        // macOS/BSD: capture goes through a /dev/bpf* clone device, whose
        // permissions can be relaxed independently of root (ChmodBPF).
        //
        // **A BPF device is exclusive: one open handle each.** So "could not open
        // /dev/bpf0" says nothing about permission when some other capture already
        // holds it — libpcap itself answers this by scanning upward until it finds a
        // free one, and this probe has to do the same or it will disagree with the
        // library it is gating.
        //
        // Scanning only 0..4 made the answer depend on load. Running the suite at
        // --test-threads=100, or alongside another netget, occupies the first few
        // devices and the probe then reported "no capture access" on a host that
        // plainly had it — `is_met_by()` refuses ARP, DataLink and IS-IS at startup on
        // that answer, so those servers became un-startable whenever the machine was
        // busy. It showed up as `isis_spawn_outcome_matches_capture_privilege` failing
        // with "spawn returned Ok without capture privilege": the capture opened fine,
        // and only the probe disagreed.
        //
        // `EBUSY` is therefore *positive* evidence — the device exists and we were
        // allowed to try, it is merely in use. Only `EACCES`/`EPERM` on every device we
        // can see is a real denial.
        #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
        {
            let mut denied = false;
            for n in 0..256 {
                match std::fs::File::open(format!("/dev/bpf{}", n)) {
                    Ok(_) => {
                        debug!("Opened /dev/bpf{} - L2 capture available without root", n);
                        return true;
                    }
                    Err(e) => match e.raw_os_error() {
                        // In use by someone else: we were permitted to try, which is
                        // what the caller is actually asking about.
                        Some(libc::EBUSY) => {
                            debug!("/dev/bpf{} is busy - capture access is present", n);
                            return true;
                        }
                        Some(libc::EACCES) | Some(libc::EPERM) => {
                            denied = true;
                            continue;
                        }
                        // Past the last device the system created.
                        Some(libc::ENOENT) | Some(libc::ENXIO) => break,
                        _ => continue,
                    },
                }
            }
            if denied {
                debug!("every /dev/bpf* refused permission - no L2 capture access");
            }
        }

        debug!(
            "No capture handle available ({}) and not root - capture protocols \
             (ARP, DataLink, IS-IS) will be refused at startup.",
            std::io::Error::last_os_error()
        );
        false
    }

    #[cfg(windows)]
    {
        // Npcap/WinPcap driver access requires Administrator.
        is_running_as_root()
    }
}

/// Whether any of the given paths exists (used by the Linux device probes).
#[cfg(target_os = "linux")]
fn any_path_exists(paths: &[&str]) -> bool {
    paths.iter().any(|p| std::path::Path::new(p).exists())
}

/// Probe for a usable Bluetooth adapter.
///
/// Device probes are deliberately **permissive**: they return `false` only on
/// positive evidence that the stack is absent, never on "could not tell". Refusing
/// a start is a hard failure, and the alternative — the protocol failing later with
/// its own diagnostic — is strictly better than refusing a user who does have
/// access.
fn has_bluetooth_capability() -> bool {
    #[cfg(target_os = "linux")]
    {
        // BlueZ is reached over the D-Bus system bus, and an adapter shows up under
        // /sys/class/bluetooth. Both are needed; neither requires root.
        let bus = any_path_exists(&[
            "/run/dbus/system_bus_socket",
            "/var/run/dbus/system_bus_socket",
        ]);
        let adapter = std::fs::read_dir("/sys/class/bluetooth")
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if !bus {
            debug!("No D-Bus system bus socket - BlueZ is unreachable");
        } else if !adapter {
            debug!("No adapter under /sys/class/bluetooth");
        }
        bus && adapter
    }

    // macOS gates CoreBluetooth through TCC, which cannot be probed without
    // triggering the permission prompt, so assume available and let the protocol
    // report the real error. Same for any platform we have no probe for.
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Probe for usable USB device access (see [`has_bluetooth_capability`] on why
/// these probes are permissive).
fn has_usb_capability() -> bool {
    #[cfg(target_os = "linux")]
    {
        // libusb enumerates through /dev/bus/usb; per-device write access is a udev
        // matter we cannot check generically, so existence is the whole test.
        let present = std::path::Path::new("/dev/bus/usb").is_dir();
        if !present {
            debug!("/dev/bus/usb is absent - usbfs is not mounted");
        }
        present
    }

    // macOS reaches USB through IOKit, which needs no elevation for the device
    // classes NetGet uses.
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// Probe for a usable NFC/PC-SC reader stack (see [`has_bluetooth_capability`] on
/// why these probes are permissive).
fn has_nfc_capability() -> bool {
    #[cfg(target_os = "linux")]
    {
        // The pcsc crate talks to pcscd over its socket; without the daemon running
        // there is nothing to talk to.
        let present = any_path_exists(&["/run/pcscd/pcscd.comm", "/var/run/pcscd/pcscd.comm"]);
        if !present {
            debug!("No pcscd socket - the PC/SC daemon is not running");
        }
        present
    }

    // macOS ships the PCSC framework; other platforms are not probed.
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
