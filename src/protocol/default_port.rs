//! The port a server starts on when the caller names none.
//!
//! One decision, used everywhere a port can be left out: the dashboard's picker and create
//! form, `ServerForm`, MCP `start_server`, the model's `open_server` and `--server`. All of
//! them reach it through [`crate::cli::server_startup::start_server_from_action`] or, for the
//! picker's preview, through [`default_port_for_server`] directly, so the port a preview names
//! is the port a start takes.
//!
//! The rule:
//!
//! 1. **An explicit port is never replaced** — `0` included. This module is consulted only
//!    when the caller passed no port at all.
//! 2. The protocol's declared [`ProtocolMetadataV2::well_known_port`] when it is 1024 or above,
//!    or when it is below 1024 and this process can bind privileged ports
//!    ([`SystemCapabilities::can_bind_privileged_ports`], the existing probe — not a new one).
//! 3. Otherwise `0`, an OS-assigned port, **with the reason**: the privileged port this
//!    process cannot bind, or the port something else already holds.
//!
//! A port already taken falls back rather than failing, because the caller who named no port
//! asked for "a" server, not for that number — and the one who needs the number passes it, in
//! which case a taken port is an error they should see.
//!
//! The in-use check binds and drops a probe socket on the address the server would use. That
//! is the probe-then-bind pattern `server_startup.rs` removed for port 0, and it is safe here
//! for the reason it was not there: a fixed port cannot be handed to someone else between the
//! probe and the real bind. If another process takes it in that window, the real bind fails
//! with its own "address in use", which is the same error the caller would have had anyway.
//!
//! [`ProtocolMetadataV2::well_known_port`]: crate::protocol::metadata::ProtocolMetadataV2::well_known_port

use crate::llm::actions::Server;
use crate::privilege::SystemCapabilities;
use crate::protocol::metadata::PortTransport;

/// The host a socket server binds when the caller and the protocol name none.
///
/// The same loopback default `server_startup.rs` falls back to, so the in-use probe asks about
/// the address the server will actually take.
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// Why a server is not starting on its well-known port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortFallback {
    /// The port is below 1024 and this process cannot bind privileged ports.
    NeedsPrivilege,
    /// Something already holds the port on the address the server would bind.
    InUse,
}

/// The resolved default for one server start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultPort {
    /// The port to bind. `0` asks the OS for a free one.
    pub port: u16,
    /// The protocol's declared well-known port, if it has one.
    pub well_known: Option<u16>,
    /// The transport that port is registered for.
    pub transport: PortTransport,
    /// Set when the well-known port exists but is not being used, and why.
    pub fallback: Option<PortFallback>,
}

impl DefaultPort {
    /// One phrase for a picker line, a form's help text or a status line.
    ///
    /// Always names the well-known port when there is one, including when it is not being used,
    /// so the reader learns both the number and why it was passed over.
    pub fn describe(&self) -> String {
        match (self.well_known, self.fallback) {
            (Some(p), None) => format!("well-known port {p}"),
            (Some(p), Some(PortFallback::NeedsPrivilege)) => {
                format!("well-known port {p} needs root; starting on an OS-assigned port")
            }
            (Some(p), Some(PortFallback::InUse)) => {
                format!("well-known port {p} is in use; starting on an OS-assigned port")
            }
            (None, _) if self.port == 0 => {
                "no well-known port; starting on an OS-assigned port".to_string()
            }
            (None, _) => format!("port {}", self.port),
        }
    }
}

/// Resolve the default port from a declared well-known port.
///
/// Pure except for the in-use probe; see the module documentation for the rule.
pub fn resolve_default_port(
    well_known: Option<u16>,
    transport: PortTransport,
    host: &str,
    caps: &SystemCapabilities,
) -> DefaultPort {
    let Some(port) = well_known.filter(|p| *p != 0) else {
        return DefaultPort {
            port: 0,
            well_known: None,
            transport,
            fallback: None,
        };
    };
    let fallback = if port < 1024 && !caps.can_bind_privileged_ports {
        Some(PortFallback::NeedsPrivilege)
    } else if port_is_in_use(host, port, transport) {
        Some(PortFallback::InUse)
    } else {
        None
    };
    DefaultPort {
        port: if fallback.is_some() { 0 } else { port },
        well_known: Some(port),
        transport,
        fallback,
    }
}

/// Whether `host:port` is already held by someone else, for the given transport.
///
/// Only an `AddrInUse` answer counts. Every other failure — permission, an address that is not
/// ours, a platform without sockets (the wasm build's virtual network) — says nothing about
/// whether the port is taken, and the real bind will report it precisely.
pub fn port_is_in_use(host: &str, port: u16, transport: PortTransport) -> bool {
    let result = match transport {
        PortTransport::Tcp => std::net::TcpListener::bind((host, port)).map(drop),
        PortTransport::Udp => std::net::UdpSocket::bind((host, port)).map(drop),
        // The standard library has no SCTP socket; the real bind reports a taken port.
        PortTransport::Sctp => return false,
    };
    matches!(result, Err(e) if e.kind() == std::io::ErrorKind::AddrInUse)
}

/// The default port for a server protocol when the caller names none, or `None` when a port
/// means nothing to it (an interface, a pipe, a pty).
///
/// - A declared well-known port goes through [`resolve_default_port`].
/// - Otherwise the protocol's own `default_binding()` port, when it declares a binding: `None`
///   there is the protocol saying it binds no port at all.
/// - Otherwise `0`. A protocol with neither is a socket server with no port of its own, and an
///   OS-assigned port is the only default that cannot collide.
///
/// `host` is the address the caller asked for; `None` means the protocol's own default host,
/// then [`DEFAULT_HOST`].
pub fn default_port_for_server(
    protocol: &dyn Server,
    host: Option<&str>,
    caps: &SystemCapabilities,
) -> Option<DefaultPort> {
    let metadata = protocol.metadata();
    let binding = protocol.default_binding();
    if let Some(well_known) = metadata.well_known_port {
        let binding_host = binding.as_ref().and_then(|b| b.host.clone());
        let host = host
            .map(str::to_string)
            .or(binding_host)
            .unwrap_or_else(|| DEFAULT_HOST.to_string());
        return Some(resolve_default_port(
            Some(well_known),
            metadata.well_known_transport,
            &host,
            caps,
        ));
    }
    let port = match binding {
        Some(b) => b.port?,
        None => 0,
    };
    Some(DefaultPort {
        port,
        well_known: None,
        transport: metadata.well_known_transport,
        fallback: None,
    })
}
