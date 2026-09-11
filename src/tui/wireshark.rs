//! `[ view in wireshark ]` — a paste-ready capture command for one instance.
//!
//! NetGet does not write pcaps; what it can do is tell Wireshark exactly where
//! to look. Given what the dashboard already knows about an instance (its
//! protocol, the address it binds or dials, an interface for the raw-socket
//! protocols) this module derives:
//!
//! * the **interface** to listen on (loopback for local addresses, the
//!   platform's "everything" device otherwise),
//! * a **capture filter** (BPF, applied at capture time — cheap),
//! * a **display filter** (Wireshark syntax — names the dissector, so the
//!   packet list shows decoded `HTTP`/`DNS`/`MQTT` rows, not `TCP`),
//! * a `-d` **decode-as** clause, because most instances sit on a port
//!   Wireshark would not guess the protocol for (an HTTP server on 8080 is
//!   just TCP to it until told otherwise),
//! * and the `wireshark` / `tshark` command lines that put those together.
//!
//! Everything here is pure and platform-parameterised so it can be asserted in
//! tests; the only system knowledge is a table of which NetGet protocol rides
//! on which transport and what Wireshark calls its dissector. A protocol
//! missing from the table still gets a correct, if undecoded, capture.
//!
//! The form offers the same thing **before** the instance exists, so the
//! capture can be running when the first packet arrives — the whole point of
//! showing it at creation time.

use std::fmt::Write as _;

/// What the protocol rides on, which decides both filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
    /// Served on both (DNS, SIP, syslog): filter on the port, either transport.
    TcpOrUdp,
    /// An IP-level or link-level protocol with no port. The payload is the
    /// BPF keyword/expression that selects it.
    Raw(&'static str),
    /// Not on an IP network at all — USB, Bluetooth, a pty. Wireshark can
    /// sometimes still see it, but not through a network interface.
    NotNetwork,
}

/// How Wireshark should treat one protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wire {
    pub transport: Transport,
    /// Dissector name for `-d <transport>.port==N,<name>`; `None` means no
    /// application-layer dissector applies (plain TCP/UDP) or the protocol has
    /// no port.
    pub decode_as: Option<&'static str>,
    /// Display-filter expression for the application layer, when it differs
    /// from `decode_as` (`nbss` decodes SMB, but you filter on `smb2`).
    pub display: Option<&'static str>,
    /// A sentence for the protocols Wireshark cannot reach via an interface.
    pub note: Option<&'static str>,
}

const fn tcp(decode_as: &'static str) -> Wire {
    Wire {
        transport: Transport::Tcp,
        decode_as: Some(decode_as),
        display: None,
        note: None,
    }
}

const fn udp(decode_as: &'static str) -> Wire {
    Wire {
        transport: Transport::Udp,
        decode_as: Some(decode_as),
        display: None,
        note: None,
    }
}

const fn either(decode_as: &'static str) -> Wire {
    Wire {
        transport: Transport::TcpOrUdp,
        decode_as: Some(decode_as),
        display: None,
        note: None,
    }
}

const fn raw(bpf: &'static str, display: &'static str) -> Wire {
    Wire {
        transport: Transport::Raw(bpf),
        decode_as: None,
        display: Some(display),
        note: None,
    }
}

const fn offline(note: &'static str) -> Wire {
    Wire {
        transport: Transport::NotNetwork,
        decode_as: None,
        display: None,
        note: Some(note),
    }
}

const fn with_display(wire: Wire, display: &'static str) -> Wire {
    Wire {
        display: Some(display),
        ..wire
    }
}

/// Attach a caveat to an otherwise-correct entry.
///
/// Distinct from [`offline`], which says "there is no capture command for this at all".
/// These protocols *do* have a working command; the note warns about a way the obvious
/// capture still shows you the wrong thing.
const fn with_note(wire: Wire, note: &'static str) -> Wire {
    Wire {
        note: Some(note),
        ..wire
    }
}

const PLAIN_TCP: Wire = Wire {
    transport: Transport::Tcp,
    decode_as: None,
    display: None,
    note: None,
};

const PLAIN_UDP: Wire = Wire {
    transport: Transport::Udp,
    decode_as: None,
    display: None,
    note: None,
};

const TFTP_TID_NOTE: &str = "A `udp port 69` filter captures only the initial RRQ/WRQ. TFTP \
    then moves the transfer to an ephemeral TID port, so every DATA and ACK packet is missed. \
    Capture the host instead (drop the port from the filter) and use the `tftp` display filter.";

const ARP_LOOPBACK_NOTE: &str = "`arp` is an Ethernet-only BPF keyword and is rejected on \
    loopback, which is DLT_NULL on macOS. Capture on a real interface; ARP is not carried on lo0 \
    at all, so there would be nothing to see there anyway. Same trap as isis.";

const QUIC_ALPN_NOTE: &str = "NetGet's QUIC server negotiates ALPN `h3` while sending raw stream \
    bytes rather than RFC 9114 frames, so Wireshark hands the payload to its HTTP/3 sub-dissector \
    and reports malformed frames. That is expected - read the QUIC stream payload directly.";

const USB_NOTE: &str = "USB is not network traffic. Wireshark can capture it from usbmon on \
                        Linux (tshark -D lists usbmonN) or the XHC20 device on macOS after \
                        `sudo ifconfig XHC20 up`; the USB/IP socket NetGet exposes is plain TCP \
                        and can be watched on that port instead.";
const BLE_NOTE: &str = "Bluetooth is not network traffic. On Linux use btmon or Wireshark's \
                        bluetooth-monitor interface; on macOS enable PacketLogger from the \
                        Bluetooth developer tools and open its .pklg in Wireshark.";
const LOCAL_NOTE: &str = "This protocol runs over a local descriptor (pty, pipe, socket file, \
                          stdio), which never crosses a network interface. Use `strace -e \
                          trace=read,write` / `dtruss` on the process, or `socat -v` in front \
                          of the socket, to watch it.";

/// NetGet protocol name (as `protocol_name()` reports it, any case) → wire.
///
/// A name not listed here is plain TCP: every protocol in the registry that
/// is not UDP, raw or off-network speaks TCP, so the default is right far
/// more often than it is wrong, and the worst case is an undecoded capture.
pub fn wire_for(protocol: &str) -> Wire {
    let name = protocol.trim().to_ascii_lowercase().replace('-', "_");
    match name.as_str() {
        // ---- transports --------------------------------------------------
        "tcp" | "reverse_shell" | "dc" | "zookeeper" | "svn" => PLAIN_TCP,
        "udp" => PLAIN_UDP,
        "tls" | "dot" | "tor_relay" => tcp("tls"),
        "quic" => with_note(udp("quic"), QUIC_ALPN_NOTE),
        // The discovery family. All three are UDP and all three were falling through to the
        // PLAIN_TCP default, which is simply the wrong transport. Dissector names checked
        // against this machine's tshark: present in `-G protocols` and accepted by
        // `-d udp.port==N,<name>` (a bogus name is rejected there, so the check is real).
        // The DynamoDB *client* reports protocol_name() "DynamoDB" -> "dynamodb", which the
        // web arm above does not list (it has only the server's "dynamo"), so it fell through
        // to PLAIN_TCP and lost the HTTP dissector.
        "dynamodb" => tcp("http"),
        // Client-only protocol names. Both talk HTTP to a provider and may use TLS, so the
        // display filter admits either; without an arm they defaulted to plain TCP.
        "openidconnect" => with_display(tcp("http"), "http || tls"),
        "saml" => with_display(tcp("http"), "http || tls"),
        "ssdp" => udp("ssdp"),
        "llmnr" => udp("llmnr"),
        "netbios_ns" => udp("nbns"),
        // http3 is a client-only protocol name, and it reaches wire_for through
        // CaptureTarget::client. Without an arm it defaulted to plain TCP; it is QUIC.
        "http3" => with_display(udp("quic"), "http3 || quic"),
        // ---- web ---------------------------------------------------------
        "http" | "websocket" | "proxy" | "webdav" | "jsonrpc" | "xmlrpc" | "openapi" | "openai"
        | "ollama" | "mcp" | "oauth2" | "openid" | "saml_idp" | "saml_sp" | "s3" | "sqs"
        | "dynamo" | "elasticsearch" | "couchdb" | "kubernetes" | "oci_registry" | "npm"
        | "pypi" | "maven" | "rss" | "hls" | "yarn" | "spark" | "snowflake" | "mercurial"
        | "webrtc_signaling" | "torrent_tracker" => tcp("http"),
        "doh" => tcp("tls"),
        "http2" => tcp("http2"),
        "grpc" | "etcd" => with_display(tcp("http2"), "grpc || http2"),
        // ---- mail / text -------------------------------------------------
        "smtp" => tcp("smtp"),
        "pop3" => tcp("pop"),
        "imap" => tcp("imap"),
        "nntp" => tcp("nntp"),
        "irc" => tcp("irc"),
        "xmpp" => tcp("xmpp"),
        "telnet" => tcp("telnet"),
        "ssh" => tcp("ssh"),
        "ftp" => tcp("ftp"),
        "whois" => tcp("whois"),
        "socks5" => tcp("socks"),
        // NetGet's git server implements Smart **HTTP** only - the payload starts
        // "GET /...info/refs?service=git-upload-pack HTTP/1.1". Wireshark's `git`
        // dissector decodes pkt-line directly over TCP, i.e. git:// on 9418, and will not
        // decode this. The trap is that our own examples use port 9418, which makes the
        // wrong entry look right.
        "git" => tcp("http"),
        // ---- databases / brokers -----------------------------------------
        "mysql" => tcp("mysql"),
        "postgresql" => tcp("pgsql"),
        "mssql" => tcp("tds"),
        "mongodb" => tcp("mongo"),
        "cassandra" => tcp("cql"),
        // DRDA registers only a heuristic dissector — it is not in the
        // `tcp.port` decode-as table, so name it in the display filter alone.
        "db2" => with_display(PLAIN_TCP, "drda"),
        "redis" => tcp("resp"),
        "memcached" => tcp("memcache"),
        "ldap" => tcp("ldap"),
        "kafka" => tcp("kafka"),
        "amqp" => tcp("amqp"),
        "mqtt" => tcp("mqtt"),
        // ---- remote desktop / files / industrial -------------------------
        "vnc" => tcp("vnc"),
        "rdp" => with_display(tcp("tpkt"), "rdp"),
        // This server writes no NetBIOS session-service header, so a `nbss` decode-as has
        // nothing to key on and never resolves. Point at smb2 directly until framing exists.
        "smb" => with_display(tcp("smb2"), "smb2 || smb"),
        "nfs" => with_display(tcp("rpc"), "nfs"),
        "modbus" => tcp("mbtcp"),
        // IPP is an HTTP payload; Wireshark reaches it through the http
        // dissector, which picks ipp by media type.
        "ipp" => with_display(tcp("http"), "ipp || http"),
        "bgp" => tcp("bgp"),
        "bitcoin" => tcp("bitcoin"),
        "torrent_peer" => tcp("bittorrent"),
        // ---- UDP ---------------------------------------------------------
        "dns" => either("dns"),
        "mdns" => udp("mdns"),
        "ntp" => udp("ntp"),
        "dhcp" | "bootp" => udp("dhcp"),
        // `udp port 69` captures only the RRQ/WRQ: TFTP then moves to an ephemeral TID
        // port for DATA/ACK, so a port-69 filter shows the request and none of the transfer.
        "tftp" => with_note(udp("tftp"), TFTP_TID_NOTE),
        "snmp" => udp("snmp"),
        "syslog" => either("syslog"),
        "radius" => udp("radius"),
        "stun" => udp("stun"),
        "turn" => with_display(udp("stun"), "stun || turnchannel"),
        "coap" => udp("coap"),
        "rip" => udp("rip"),
        "sip" => either("sip"),
        "rtp" => udp("rtp"),
        "rtsp" => tcp("rtsp"),
        "wireguard" => udp("wg"),
        "openvpn" => udp("openvpn"),
        "ipsec" => udp("isakmp"),
        "torrent_dht" => with_display(udp("bt-dht"), "bt-dht"),
        "webrtc" => with_display(udp("stun"), "stun || dtls || rtp"),
        // ---- raw / link layer --------------------------------------------
        "icmp" => raw("icmp", "icmp"),
        "igmp" => raw("igmp", "igmp"),
        "ospf" => raw("ip proto 89", "ospf"),
        // Same trap as isis: `arp` is an Ethernet-only BPF keyword and is rejected on
        // loopback (DLT_NULL on macOS). Capture on a real interface.
        "arp" => with_note(raw("arp", "arp"), ARP_LOOPBACK_NOTE),
        // `isis` is an Ethernet-only BPF keyword and is rejected outright on a
        // loopback device, so let the display filter do the selecting.
        "isis" => raw("", "isis"),
        "datalink" => raw("", ""),
        // ---- not on a network --------------------------------------------
        "pty" | "stdio" | "named_pipe" | "socket_file" | "ssh_agent" => offline(LOCAL_NOTE),
        "nfc" => offline(
            "NFC goes through a PC/SC reader, not a network interface; Wireshark cannot see it.",
        ),
        n if n.starts_with("usb") => offline(USB_NOTE),
        n if n.starts_with("bluetooth") => offline(BLE_NOTE),
        _ => PLAIN_TCP,
    }
}

/// The operating system the command will be pasted into; it decides interface
/// names and the privilege advice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "linux") {
            Platform::Linux
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Other
        }
    }

    fn loopback(self) -> &'static str {
        match self {
            Platform::MacOs => "lo0",
            Platform::Linux => "lo",
            Platform::Windows => "\\Device\\NPF_Loopback",
            Platform::Other => "lo0",
        }
    }

    /// The device that sees every interface, where one exists.
    fn any(self) -> Option<&'static str> {
        match self {
            Platform::Linux => Some("any"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The filter is on the port NetGet binds.
    Server,
    /// The filter is on the port NetGet dials; its own source port is
    /// ephemeral and unknown until connected.
    Client,
}

/// What the dashboard knows about the instance (or the form about the one
/// being created).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureTarget {
    pub protocol: String,
    pub role: Role,
    /// Bind host (server) or remote host (client). `None` means the
    /// protocol's default, which is loopback for every port-based protocol.
    pub host: Option<String>,
    /// Bind port or remote port. `None`/`0` means unknown until the server
    /// starts.
    pub port: Option<u16>,
    /// Explicit interface, for the raw-socket protocols.
    pub interface: Option<String>,
}

impl CaptureTarget {
    /// Split a `host:port` / `[v6]:port` / bare-host remote address.
    pub fn client(protocol: &str, remote_addr: Option<&str>) -> Self {
        let (host, port) = match remote_addr.map(str::trim).filter(|s| !s.is_empty()) {
            None => (None, None),
            Some(addr) => split_host_port(addr),
        };
        Self {
            protocol: protocol.to_string(),
            role: Role::Client,
            host,
            port,
            interface: None,
        }
    }
}

/// `host:port` → (host, port). Tolerates `[::1]:53`, a bare host, and a bare
/// IPv6 address (which has many colons and no port).
fn split_host_port(addr: &str) -> (Option<String>, Option<u16>) {
    if let Some(rest) = addr.strip_prefix('[') {
        if let Some((host, port)) = rest.split_once(']') {
            let port = port.strip_prefix(':').and_then(|p| p.parse().ok());
            return (Some(host.to_string()), port);
        }
    }
    if addr.matches(':').count() == 1 {
        if let Some((host, port)) = addr.rsplit_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                let host = (!host.is_empty()).then(|| host.to_string());
                return (host, Some(port));
            }
        }
    }
    (Some(addr.to_string()), None)
}

fn is_loopback(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h.is_empty()
        || h.eq_ignore_ascii_case("localhost")
        || h == "::1"
        || h.starts_with("127.")
        || h == "0:0:0:0:0:0:0:1"
}

fn is_unspecified(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    h == "0.0.0.0" || h == "::" || h == "0:0:0:0:0:0:0:0"
}

/// One line of the modal, typed so the renderer can style it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanLine {
    Heading(String),
    /// A value meant to be copied verbatim.
    Value(String),
    Note(String),
    Blank,
}

/// The derived capture recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturePlan {
    pub target: CaptureTarget,
    pub wire: Wire,
    pub interface: String,
    pub capture_filter: String,
    pub display_filter: String,
    /// `tcp.port==8080,http` — the argument to `-d`.
    pub decode_as: Option<String>,
    pub notes: Vec<String>,
}

impl CapturePlan {
    pub fn build(target: CaptureTarget, platform: Platform) -> Self {
        let wire = wire_for(&target.protocol);
        let mut notes = Vec::new();
        let host = target.host.as_deref().map(str::trim).unwrap_or("");
        let port = target.port.filter(|p| *p != 0);
        let host_is_local = is_loopback(host);
        let host_is_any = is_unspecified(host);

        // ---- interface ----
        let interface = match wire.transport {
            Transport::Raw(_) => target
                .interface
                .as_deref()
                .map(str::trim)
                .filter(|i| !i.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| platform.loopback().to_string()),
            Transport::NotNetwork => String::new(),
            _ if host_is_local => platform.loopback().to_string(),
            _ => match platform.any() {
                Some(any) => any.to_string(),
                None => {
                    notes.push(if host_is_any {
                        format!(
                            "Bound on every address. `{}` shows loopback peers only; for remote \
                             peers replace it with the external interface (`tshark -D` lists \
                             them, usually en0).",
                            platform.loopback()
                        )
                    } else {
                        format!(
                            "`{host}` is not a local address. Replace the interface with the \
                             one that routes to it (`route -n get {host}` names it; `tshark -D` \
                             lists all)."
                        )
                    });
                    if host_is_any {
                        platform.loopback().to_string()
                    } else {
                        "en0".to_string()
                    }
                }
            },
        };

        // ---- capture filter (BPF) ----
        let host_clause = (!host_is_local && !host_is_any && !host.is_empty())
            .then(|| format!(" and host {host}"));
        let capture_filter = match wire.transport {
            Transport::Tcp => port_bpf("tcp", port, &host_clause),
            Transport::Udp => port_bpf("udp", port, &host_clause),
            Transport::TcpOrUdp => port_bpf("", port, &host_clause),
            Transport::Raw(bpf) => bpf.to_string(),
            Transport::NotNetwork => String::new(),
        };

        // ---- display filter + decode-as ----
        let app_filter = wire.display.or(wire.decode_as);
        let (display_filter, decode_as) = match wire.transport {
            Transport::NotNetwork => (String::new(), None),
            Transport::Raw(_) => (app_filter.unwrap_or("").to_string(), None),
            Transport::Tcp | Transport::Udp | Transport::TcpOrUdp => {
                let port_expr = match (wire.transport, port) {
                    (Transport::Tcp, Some(p)) => format!("tcp.port == {p}"),
                    (Transport::Udp, Some(p)) => format!("udp.port == {p}"),
                    (Transport::TcpOrUdp, Some(p)) => {
                        format!("(tcp.port == {p} || udp.port == {p})")
                    }
                    (Transport::Tcp, None) => "tcp".to_string(),
                    (Transport::Udp, None) => "udp".to_string(),
                    _ => "tcp || udp".to_string(),
                };
                let display = match app_filter {
                    Some(app) if app.contains("||") => format!("{port_expr} && ({app})"),
                    Some(app) => format!("{port_expr} && {app}"),
                    None => port_expr,
                };
                let decode_as = match (wire.decode_as, port) {
                    (Some(name), Some(p)) => {
                        let table = match wire.transport {
                            Transport::Udp => "udp.port",
                            _ => "tcp.port",
                        };
                        Some(format!("{table}=={p},{name}"))
                    }
                    _ => None,
                };
                (display, decode_as)
            }
        };

        // ---- notes ----
        if let Some(note) = wire.note {
            notes.push(note.to_string());
        }
        if !matches!(wire.transport, Transport::NotNetwork | Transport::Raw(_)) && port.is_none() {
            notes.push(match target.role {
                Role::Server => "No fixed port yet (0 lets the OS pick one at start). The filter \
                                 matches all traffic on the transport; re-open this from the \
                                 running server's row to get the real port."
                    .to_string(),
                Role::Client => "No remote port given, so the filter cannot narrow to one \
                                 connection yet."
                    .to_string(),
            });
        }
        if target.role == Role::Client && !matches!(wire.transport, Transport::NotNetwork) {
            notes.push(
                "Filtering on the remote port: the client's own source port is chosen by the OS \
                 at connect time."
                    .to_string(),
            );
        }
        if wire.transport == Transport::TcpOrUdp && wire.decode_as.is_some() {
            if let (Some(name), Some(p)) = (wire.decode_as, port) {
                notes.push(format!(
                    "Served over both transports; add `-d udp.port=={p},{name}` as well if the \
                     peer uses UDP."
                ));
            }
        }
        if !matches!(wire.transport, Transport::NotNetwork) {
            notes.push(match platform {
                Platform::MacOs => "Live capture needs /dev/bpf* access. Wireshark's installer \
                                    ships ChmodBPF for that; if capture is refused, `sudo \
                                    dseditgroup -o edit -a $USER -t user access_bpf` and log in \
                                    again."
                    .to_string(),
                Platform::Linux => "Live capture needs CAP_NET_RAW on dumpcap: `sudo \
                                    dpkg-reconfigure wireshark-common` then `sudo usermod -aG \
                                    wireshark $USER`, or run with sudo."
                    .to_string(),
                Platform::Windows => "Live capture needs Npcap with the loopback adapter enabled \
                                      (the Wireshark installer offers it)."
                    .to_string(),
                Platform::Other => {
                    "Live capture needs raw-socket privilege on this platform.".to_string()
                }
            });
        }

        Self {
            target,
            wire,
            interface,
            capture_filter,
            display_filter,
            decode_as,
            notes,
        }
    }

    fn common_args(&self, out: &mut String) {
        let _ = write!(out, " -i {}", shell_word(&self.interface));
        if !self.capture_filter.is_empty() {
            let _ = write!(out, " -f {}", shell_word(&self.capture_filter));
        }
        if !self.display_filter.is_empty() {
            let _ = write!(out, " -Y {}", shell_word(&self.display_filter));
        }
        if let Some(decode) = &self.decode_as {
            let _ = write!(out, " -d {}", shell_word(decode));
        }
    }

    /// The GUI: `-k` starts capturing immediately.
    pub fn wireshark_command(&self) -> Option<String> {
        if self.wire.transport == Transport::NotNetwork {
            return None;
        }
        let mut out = String::from("wireshark -k");
        self.common_args(&mut out);
        Some(out)
    }

    /// The terminal: `-l` flushes per packet so a pipe shows rows as they come.
    pub fn tshark_command(&self) -> Option<String> {
        if self.wire.transport == Transport::NotNetwork {
            return None;
        }
        let mut out = String::from("tshark -l");
        self.common_args(&mut out);
        Some(out)
    }

    /// The modal body.
    pub fn lines(&self) -> Vec<PlanLine> {
        let mut lines = Vec::new();
        let what = match self.target.role {
            Role::Server => "server",
            Role::Client => "client",
        };
        let mut ident = format!("{} {what}", self.target.protocol);
        if let Some(host) = self.target.host.as_deref().filter(|h| !h.trim().is_empty()) {
            let _ = write!(ident, " on {host}");
            if let Some(port) = self.target.port.filter(|p| *p != 0) {
                let _ = write!(ident, ":{port}");
            }
        } else if let Some(port) = self.target.port.filter(|p| *p != 0) {
            let _ = write!(ident, " on port {port}");
        }
        lines.push(PlanLine::Note(ident));
        lines.push(PlanLine::Blank);

        if let Some(cmd) = self.wireshark_command() {
            lines.push(PlanLine::Heading(
                "Wireshark (GUI) — paste in a terminal".into(),
            ));
            lines.push(PlanLine::Value(cmd));
            lines.push(PlanLine::Blank);
        }
        if let Some(cmd) = self.tshark_command() {
            lines.push(PlanLine::Heading("tshark (terminal)".into()));
            lines.push(PlanLine::Value(cmd));
            lines.push(PlanLine::Blank);
        }
        if self.wire.transport != Transport::NotNetwork {
            lines.push(PlanLine::Heading(
                "Pieces, for an already-open Wireshark".into(),
            ));
            lines.push(PlanLine::Note(format!(
                "interface:       {}",
                self.interface
            )));
            lines.push(PlanLine::Note(format!(
                "capture filter:  {}",
                if self.capture_filter.is_empty() {
                    "(none — every frame)"
                } else {
                    &self.capture_filter
                }
            )));
            lines.push(PlanLine::Note(format!(
                "display filter:  {}",
                if self.display_filter.is_empty() {
                    "(none)"
                } else {
                    &self.display_filter
                }
            )));
            if let Some(decode) = &self.decode_as {
                lines.push(PlanLine::Note(format!(
                    "decode as:       {decode}   (Analyze → Decode As…)"
                )));
            }
            lines.push(PlanLine::Blank);
        }
        if !self.notes.is_empty() {
            lines.push(PlanLine::Heading("Notes".into()));
            for note in &self.notes {
                lines.push(PlanLine::Note(format!("• {note}")));
            }
        }
        lines
    }
}

fn port_bpf(proto: &str, port: Option<u16>, host_clause: &Option<String>) -> String {
    let mut out = match (proto, port) {
        ("", Some(p)) => format!("port {p}"),
        ("", None) => "tcp or udp".to_string(),
        (proto, Some(p)) => format!("{proto} port {p}"),
        (proto, None) => proto.to_string(),
    };
    if let Some(clause) = host_clause {
        if proto.is_empty() && port.is_none() {
            out = format!("({out})");
        }
        out.push_str(clause);
    }
    out
}

/// Quote for a POSIX shell (and cmd.exe, for the values we produce) only when
/// needed, so `-i lo0` stays bare and `-f "tcp port 8080"` gets its quotes.
fn shell_word(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ',' | '='));
    if plain {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}
