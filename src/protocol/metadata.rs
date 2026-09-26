//! Protocol metadata definitions
//!
//! Defines metadata about protocol implementations including state and notes.

pub use crate::privilege::DeviceClass;

/// How severe a [`PrivilegeRequirement`] is, for display purposes.
///
/// Exists so renderers do not have to match the requirement enum exhaustively —
/// adding a variant should not break every place that colours it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegeSeverity {
    /// Anyone can start this
    None,
    /// Needs something the user may already have, or can be granted without root
    Elevated,
    /// Needs full root/administrator
    Root,
}

/// Privilege requirements for a protocol
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivilegeRequirement {
    /// No special privileges required
    None,
    /// Requires ability to bind to privileged ports (< 1024)
    PrivilegedPort(u16),
    /// Requires **raw IP sockets** (`SOCK_RAW`) — ICMP, IGMP, OSPF.
    ///
    /// Not the same as [`Self::PacketCapture`]: on Linux both come from
    /// `CAP_NET_RAW`, but on macOS a user in the ChmodBPF group has capture without
    /// raw sockets. Declaring the wrong one either refuses a user who can run the
    /// protocol or admits one who cannot.
    RawSockets,
    /// Requires **layer-2 capture/injection** via BPF or `AF_PACKET` — ARP,
    /// DataLink, IS-IS.
    PacketCapture,
    /// Requires access to a local hardware device class — a Bluetooth adapter, a
    /// USB device, an NFC reader.
    ///
    /// None of these is a port or a socket, and `Root` would be both false and
    /// needlessly exclusive: a desktop user typically has adapter access already.
    DeviceAccess(DeviceClass),
    /// Requires full root/administrator access
    Root,
}

impl PrivilegeRequirement {
    /// Get a human-readable description of the requirement
    pub fn description(&self) -> String {
        match self {
            Self::None => "None".to_string(),
            Self::PrivilegedPort(port) => {
                format!("Privileged port {} (requires root or capabilities)", port)
            }
            Self::RawSockets => "Raw IP socket access (requires root or CAP_NET_RAW)".to_string(),
            Self::PacketCapture => {
                "Layer-2 packet capture (requires root, CAP_NET_RAW, or BPF device access)"
                    .to_string()
            }
            Self::DeviceAccess(class) => {
                format!("Access to a {} on this host", class.as_str())
            }
            Self::Root => "Root/Administrator access required".to_string(),
        }
    }

    /// Rough severity, for display. See [`PrivilegeSeverity`].
    pub fn severity(&self) -> PrivilegeSeverity {
        match self {
            Self::None => PrivilegeSeverity::None,
            Self::PrivilegedPort(_)
            | Self::RawSockets
            | Self::PacketCapture
            | Self::DeviceAccess(_) => PrivilegeSeverity::Elevated,
            Self::Root => PrivilegeSeverity::Root,
        }
    }

    /// Check if this requirement is met by the given system capabilities
    pub fn is_met_by(&self, caps: &crate::privilege::SystemCapabilities) -> bool {
        match self {
            Self::None => true,
            Self::PrivilegedPort(_) => caps.can_bind_privileged_ports,
            Self::RawSockets => caps.has_raw_socket_access,
            Self::PacketCapture => caps.has_packet_capture_access,
            Self::DeviceAccess(class) => caps.has_device_access(*class),
            Self::Root => caps.is_root,
        }
    }
}

/// Protocol maturity and readiness state
///
/// The derived `PartialOrd`/`Ord` follow the **declaration order below**, which
/// is deliberately least-mature to most-mature: `Incomplete < Experimental <
/// Beta < Stable`. Anything that compares maturities (the `--min-stability`
/// gate, the `/stability` listing) relies on this — do not reorder the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DevelopmentState {
    /// Incomplete implementation, not functional (e.g., OpenVPN)
    /// Will not show in LLM prompts
    Incomplete,

    /// Experimental - LLM-created, not human reviewed
    /// May have limitations or bugs
    Experimental,

    /// Beta - Human reviewed, works with real clients
    /// Mostly stable but may have minor issues
    Beta,

    /// Stable - Follows real protocol specs, well-designed LLM prompting,
    /// supports scripting for automation, LLM has sufficient control
    Stable,
}

impl DevelopmentState {
    /// Get the string representation for display
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Incomplete => "Incomplete",
            Self::Experimental => "Experimental",
            Self::Beta => "Beta",
            Self::Stable => "Stable",
        }
    }

    /// All variants, least-mature first.
    ///
    /// Handy for building help text and grouped listings without hardcoding the
    /// order at each call site.
    pub const ALL: [DevelopmentState; 4] = [
        Self::Incomplete,
        Self::Experimental,
        Self::Beta,
        Self::Stable,
    ];

    /// Parse a development state from a string, case-insensitively.
    ///
    /// Accepts the four variant names in any casing (`"beta"`, `"BETA"`,
    /// `"Beta"`). Used by the `--min-stability` CLI flag. Returns `None` for an
    /// unrecognised value so callers can produce their own error message.
    pub fn parse_ci(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "incomplete" => Some(Self::Incomplete),
            "experimental" => Some(Self::Experimental),
            "beta" => Some(Self::Beta),
            "stable" => Some(Self::Stable),
            _ => None,
        }
    }
}

impl std::str::FromStr for DevelopmentState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_ci(s).ok_or_else(|| {
            format!(
                "invalid development state '{}' (expected one of: incomplete, experimental, beta, stable)",
                s
            )
        })
    }
}

/// Protocol metadata including state and notes (legacy)
#[derive(Debug, Clone)]
pub struct ProtocolMetadata {
    /// Current implementation state
    pub state: DevelopmentState,
    /// Optional notes explaining the state or limitations
    pub notes: Option<&'static str>,
    /// Privilege requirements for this protocol
    pub privilege_requirement: PrivilegeRequirement,
}

impl ProtocolMetadata {
    /// Create new metadata with just a state (no privileges required)
    pub const fn new(state: DevelopmentState) -> Self {
        Self {
            state,
            notes: None,
            privilege_requirement: PrivilegeRequirement::None,
        }
    }

    /// Create new metadata with state and notes (no privileges required)
    pub const fn with_notes(state: DevelopmentState, notes: &'static str) -> Self {
        Self {
            state,
            notes: Some(notes),
            privilege_requirement: PrivilegeRequirement::None,
        }
    }

    /// Create new metadata with state and privilege requirement
    pub const fn with_privilege(
        state: DevelopmentState,
        privilege_requirement: PrivilegeRequirement,
    ) -> Self {
        Self {
            state,
            notes: None,
            privilege_requirement,
        }
    }

    /// Create new metadata with state, notes, and privilege requirement
    pub const fn with_notes_and_privilege(
        state: DevelopmentState,
        notes: &'static str,
        privilege_requirement: PrivilegeRequirement,
    ) -> Self {
        Self {
            state,
            notes: Some(notes),
            privilege_requirement,
        }
    }

    /// Check if this protocol should be shown to the LLM
    pub fn is_available_to_llm(&self) -> bool {
        self.state != DevelopmentState::Incomplete
    }
}

/// What a server puts on the wire when the model cannot or will not answer.
///
/// **This replaces a prose list, and the prose list was wrong in both directions.** The root
/// `CLAUDE.md` names ~20 "deliberately silent" protocols; auditing them found NDP logging its
/// own *transmit* failure as `decision=model_silent` (so `grep decision=fail_closed_` found
/// nothing for a real outage), and several BLE profiles on the list by family membership rather
/// than by anyone deciding. A paragraph cannot be checked; a declaration can.
///
/// The distinction is not a style choice. It follows from one question: **is every reply this
/// protocol can send a positive assertion?** If so, inventing one on failure is worse than
/// saying nothing — `openvpn`'s only pre-TLS server message is
/// `P_CONTROL_HARD_RESET_SERVER_V2`, and sending it *is* admitting the peer, so "fixing the
/// silence" there would turn a backend outage into an authentication bypass. ARP would write a
/// fabricated MAC into a stranger's neighbour cache. If instead the protocol has an error
/// vocabulary — an HTTP status, a RESP `-LOADING`, a Modbus exception — silence just makes the
/// peer wait out its own timeout, and answering is strictly better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureMode {
    /// The protocol answers, in its own error vocabulary, with a
    /// [`crate::utils::wire_failure`] *category* — never the error text.
    ///
    /// Keep the two categories distinct where the protocol can express them (503 vs 500, RESP
    /// `LOADING` vs `ERR`, MySQL 1205 vs 1105): a client that can tell "overloaded" from
    /// "broken" backs off instead of recording a permanent fault.
    Answers,

    /// The protocol writes **nothing**, because every frame it could send would assert
    /// something it does not know to be true.
    ///
    /// A protocol declaring this still owes the log the distinction the wire cannot carry:
    /// `decision=model_reject` / `model_silent` / `fail_closed_llm_error`, as `src/server/radius/`
    /// does. Silence on the wire is not silence in the log.
    DeliberatelySilent,
}

/// The transport a protocol's [`ProtocolMetadataV2::well_known_port`] is registered for.
///
/// Not the same thing as [`ProtocolMetadataV2::connectionless`]: that says whether the runtime
/// may reap idle peers, and TFTP is UDP yet must not be reaped. This says which socket type to
/// probe when asking "is the well-known port already taken?" — a UDP listener on 53 says nothing
/// about a TCP bind on 53.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortTransport {
    Tcp,
    Udp,
    /// SCTP (M3UA). No in-use probe is attempted for it: the standard library has no SCTP
    /// socket, and the real bind reports a taken port on its own.
    Sctp,
}

impl PortTransport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Sctp => "sctp",
        }
    }
}

/// Enhanced protocol metadata with detailed implementation information
#[derive(Debug, Clone)]
pub struct ProtocolMetadataV2 {
    /// Current maturity/readiness state
    pub state: DevelopmentState,

    /// Privilege requirements for this protocol
    pub privilege_requirement: PrivilegeRequirement,

    /// Freeform description of implementation approach
    /// Examples:
    /// - "hyper v1.0 web server library"
    /// - "russh v0.40 with SFTP support"
    /// - "Manual NTP packet parser with 48-byte construction"
    /// - "defguard_wireguard_rs v0.7 - creates real TUN interfaces"
    /// - "Custom Tor OR protocol with ntor handshake - 2,182 LOC"
    pub implementation: &'static str,

    /// Freeform description of what the LLM controls
    /// Examples:
    /// - "Full byte stream control"
    /// - "Response content (status, headers, body)"
    /// - "Authentication decisions + shell responses + SFTP operations"
    /// - "Time responses (stratum, timestamps)"
    /// - "Query responses (result sets, OK, errors)"
    /// - "No LLM control - direct Ollama delegation"
    /// - "Observation only - no LLM interaction"
    pub llm_control: &'static str,

    /// Freeform description of E2E testing approach
    /// Examples:
    /// - "reqwest HTTP client"
    /// - "ssh2 crate (libssh2 bindings)"
    /// - "OpenSSH ssh command"
    /// - "Manual NTP packet construction"
    /// - "tokio-postgres client"
    /// - "Not yet implemented"
    /// - "N/A (honeypot only)"
    pub e2e_testing: &'static str,

    /// Optional notes about limitations or special features
    pub notes: Option<&'static str>,

    /// The protocol has no connection lifecycle of its own: its "connections"
    /// are per-remote-address bookkeeping entries (UDP, raw IP, link-level)
    /// that nothing ever closes, so the runtime reaps them after
    /// `last_activity` goes stale.
    ///
    /// Leave `false` for anything connection-oriented. The reaper used to run
    /// over every server, and an idle TCP-style connection — a telnet peer
    /// waiting ten seconds for a human's manual answer — was evicted and shown
    /// as closed while its socket was perfectly alive.
    pub connectionless: bool,

    /// What this server puts on the wire when the model cannot or will not answer.
    ///
    /// Defaults to [`FailureMode::Answers`], which is the right default: a protocol that has an
    /// error vocabulary and stays silent leaves its peer to wait out a timeout, and that is the
    /// failure the August 2026 sweep found in 64 of 135 servers. Declaring
    /// [`FailureMode::DeliberatelySilent`] is an assertion that every frame this protocol could
    /// send would be a claim it cannot support — say why in `notes`.
    pub failure_mode: FailureMode,

    /// The largest single inbound message this server will buffer from a peer, in bytes.
    ///
    /// `None` means the protocol reads nothing whose length a peer chooses: a fixed-size frame
    /// (ARP, BOOTP), a single `recv_from` into a fixed buffer, or a profile that delegates its
    /// whole read loop to another protocol. `None` is a claim, not an absence — a protocol that
    /// *does* accumulate and declares `None` is the unbounded-read defect, and
    /// `tests/max_inbound_bytes_declaration_test.rs` is the ratchet that says so.
    ///
    /// **The number must be the one the code enforces**, at the point the length is decided —
    /// not an aspiration. The value exists to be greppable and testable: every declaration
    /// should have a test that sends this many bytes plus one and asserts the refusal happens
    /// *before* any model call. A bound nobody tested is a comment.
    ///
    /// Bound the size the peer **declared**, before doing arithmetic on it. NATS's `HPUB` limit
    /// was applied to `total − header`, which leaves `header` unbounded and lets `header ==
    /// total` pass every check with a zero-length body — thirty bytes on the wire buffering
    /// toward 4 GB.
    pub max_inbound_bytes: Option<usize>,

    /// The port this protocol is registered for — IANA's registry, or where IANA has none, the
    /// default its own specification or reference implementation documents (Elasticsearch
    /// 9200, `hg serve` 8000). Never a "developer" port like 8080 standing in for HTTP's 80.
    ///
    /// `None` is a claim, not an absence: the protocol has no port of its own — raw sockets,
    /// link layer, a device, a pipe, or an application protocol carried on plain HTTP whose
    /// port is HTTP's. `tests/well_known_port_declaration_test.rs` requires every server to
    /// declare one or be listed there with the reason.
    ///
    /// This is what a server starts on when the caller names no port, subject to
    /// [`crate::protocol::default_port::resolve_default_port`]: below 1024 it needs privilege,
    /// and a port already taken falls back to an OS-assigned one. An explicit port — `0`
    /// included — is never replaced.
    pub well_known_port: Option<u16>,

    /// The transport [`Self::well_known_port`] is registered for. Meaningless when that is
    /// `None`; the builder's default is TCP.
    pub well_known_transport: PortTransport,
}

impl ProtocolMetadataV2 {
    /// Create a new builder for protocol metadata
    pub const fn builder() -> ProtocolMetadataV2Builder {
        ProtocolMetadataV2Builder::new()
    }

    /// Check if this protocol should be shown to the LLM
    pub fn is_available_to_llm(&self) -> bool {
        self.state != DevelopmentState::Incomplete
    }

    /// Get a human-readable summary
    pub fn summary(&self) -> String {
        format!(
            "{} - {} - LLM: {}",
            self.state.as_str(),
            self.implementation,
            self.llm_control
        )
    }
}

/// Builder for constructing ProtocolMetadataV2
pub struct ProtocolMetadataV2Builder {
    state: DevelopmentState,
    privilege_requirement: PrivilegeRequirement,
    implementation: &'static str,
    llm_control: &'static str,
    e2e_testing: &'static str,
    notes: Option<&'static str>,
    connectionless: bool,
    failure_mode: FailureMode,
    max_inbound_bytes: Option<usize>,
    well_known_port: Option<u16>,
    well_known_transport: PortTransport,
}

impl Default for ProtocolMetadataV2Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolMetadataV2Builder {
    pub const fn new() -> Self {
        Self {
            state: DevelopmentState::Experimental,
            privilege_requirement: PrivilegeRequirement::None,
            implementation: "",
            llm_control: "",
            e2e_testing: "",
            notes: None,
            connectionless: false,
            failure_mode: FailureMode::Answers,
            max_inbound_bytes: None,
            well_known_port: None,
            well_known_transport: PortTransport::Tcp,
        }
    }

    pub const fn state(mut self, state: DevelopmentState) -> Self {
        self.state = state;
        self
    }

    pub const fn privilege_requirement(mut self, req: PrivilegeRequirement) -> Self {
        self.privilege_requirement = req;
        self
    }

    pub const fn implementation(mut self, desc: &'static str) -> Self {
        self.implementation = desc;
        self
    }

    pub const fn llm_control(mut self, desc: &'static str) -> Self {
        self.llm_control = desc;
        self
    }

    pub const fn e2e_testing(mut self, desc: &'static str) -> Self {
        self.e2e_testing = desc;
        self
    }

    pub const fn notes(mut self, notes: &'static str) -> Self {
        self.notes = Some(notes);
        self
    }

    /// Declare that this protocol writes nothing when the model cannot answer — see
    /// [`FailureMode::DeliberatelySilent`]. Put the reason in `notes`.
    pub const fn deliberately_silent(mut self) -> Self {
        self.failure_mode = FailureMode::DeliberatelySilent;
        self
    }

    /// Declare that this protocol answers the peer when the model cannot — see
    /// [`FailureMode::Answers`].
    ///
    /// `Answers` is already the default, so this changes no value. It records that someone
    /// looked at the failure path and decided, which is what
    /// `tests/failure_mode_declaration_test.rs` requires of every connectionless server: on a
    /// datagram protocol there is no connection to close, so silence is what a failure produces
    /// *unless* someone wrote the answer, and a default cannot tell those two apart. Put what
    /// the peer receives in a comment beside the call.
    pub const fn answers_on_failure(mut self) -> Self {
        self.failure_mode = FailureMode::Answers;
        self
    }

    /// Mark the protocol connectionless — see [`ProtocolMetadataV2::connectionless`].
    pub const fn connectionless(mut self) -> Self {
        self.connectionless = true;
        self
    }

    /// Declare the largest single inbound message this server buffers from a peer — see
    /// [`ProtocolMetadataV2::max_inbound_bytes`].
    ///
    /// Pass the constant the code actually enforces, so the declaration moves when the bound
    /// does: `.max_inbound_bytes(MAX_REQUEST_BYTES)`, never a literal repeated from it.
    pub const fn max_inbound_bytes(mut self, bytes: usize) -> Self {
        self.max_inbound_bytes = Some(bytes);
        self
    }

    /// Declare the TCP port this protocol is registered for — see
    /// [`ProtocolMetadataV2::well_known_port`].
    pub const fn well_known_port(mut self, port: u16) -> Self {
        self.well_known_port = Some(port);
        self.well_known_transport = PortTransport::Tcp;
        self
    }

    /// Declare the UDP port this protocol is registered for — see
    /// [`ProtocolMetadataV2::well_known_port`].
    pub const fn well_known_udp_port(mut self, port: u16) -> Self {
        self.well_known_port = Some(port);
        self.well_known_transport = PortTransport::Udp;
        self
    }

    /// Declare the SCTP port this protocol is registered for — see
    /// [`ProtocolMetadataV2::well_known_port`].
    pub const fn well_known_sctp_port(mut self, port: u16) -> Self {
        self.well_known_port = Some(port);
        self.well_known_transport = PortTransport::Sctp;
        self
    }

    pub const fn build(self) -> ProtocolMetadataV2 {
        ProtocolMetadataV2 {
            state: self.state,
            privilege_requirement: self.privilege_requirement,
            implementation: self.implementation,
            llm_control: self.llm_control,
            e2e_testing: self.e2e_testing,
            notes: self.notes,
            connectionless: self.connectionless,
            failure_mode: self.failure_mode,
            max_inbound_bytes: self.max_inbound_bytes,
            well_known_port: self.well_known_port,
            well_known_transport: self.well_known_transport,
        }
    }
}
