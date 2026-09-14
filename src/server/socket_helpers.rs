//! Socket helper utilities for creating sockets with proper options

use anyhow::Result;
#[cfg(not(target_arch = "wasm32"))]
use socket2::{Domain, Socket, Type};
#[cfg(unix)]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::io::FromRawFd;
use tokio::net::{TcpListener, UdpSocket};

/// OSPF protocol number (IPPROTO_OSPFIGP)
#[cfg(unix)]
const IPPROTO_OSPF: i32 = 89;

/// AllSPFRouters multicast group
#[cfg(unix)]
const OSPF_ALL_SPF_ROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 5);

/// AllDRouters multicast group
#[cfg(unix)]
const OSPF_ALL_DROUTERS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 6);

/// Create a TCP listener with SO_REUSEADDR enabled
///
/// This allows immediate port reuse after stopping a server,
/// which is essential for quick restart workflows in the TUI.
#[cfg(not(target_arch = "wasm32"))]
pub async fn create_reusable_tcp_listener(addr: SocketAddr) -> Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        Socket::new(Domain::IPV4, Type::STREAM, None)?
    } else {
        Socket::new(Domain::IPV6, Type::STREAM, None)?
    };

    // Enable SO_REUSEADDR for immediate port reuse
    socket.set_reuse_address(true)?;

    // Bind to the address
    socket.bind(&addr.into())?;

    // Start listening
    socket.listen(128)?;

    // Convert to tokio TcpListener
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    let tokio_listener = TcpListener::from_std(std_listener)?;

    Ok(tokio_listener)
}

/// Create a UDP socket with SO_REUSEADDR enabled
///
/// This allows immediate port reuse after stopping a server.
#[cfg(not(target_arch = "wasm32"))]
pub async fn create_reusable_udp_socket(addr: SocketAddr) -> Result<UdpSocket> {
    let socket = if addr.is_ipv4() {
        Socket::new(Domain::IPV4, Type::DGRAM, None)?
    } else {
        Socket::new(Domain::IPV6, Type::DGRAM, None)?
    };

    // Enable SO_REUSEADDR for immediate port reuse
    socket.set_reuse_address(true)?;

    // Bind to the address
    socket.bind(&addr.into())?;

    // Convert to tokio UdpSocket
    socket.set_nonblocking(true)?;
    let std_socket: std::net::UdpSocket = socket.into();
    let tokio_socket = UdpSocket::from_std(std_socket)?;

    Ok(tokio_socket)
}

/// On the browser's virtual network (`crates/netget-tokio-wasm`) there are no socket options
/// to set; binding is registering the port.
#[cfg(target_arch = "wasm32")]
pub async fn create_reusable_tcp_listener(addr: SocketAddr) -> Result<TcpListener> {
    Ok(TcpListener::bind(addr).await?)
}

/// UDP has no virtual counterpart; this reports `Unsupported` from the shim.
#[cfg(target_arch = "wasm32")]
pub async fn create_reusable_udp_socket(addr: SocketAddr) -> Result<UdpSocket> {
    Ok(UdpSocket::bind(addr).await?)
}

/// Create a raw IP socket for OSPF (protocol 89)
///
/// This creates a raw socket that receives OSPF packets directly from IP layer.
/// Requires root/CAP_NET_RAW privileges.
///
/// # Arguments
/// * `interface_addr` - IP address of the interface to bind to
/// * `join_all_routers` - Whether to join AllSPFRouters multicast group (224.0.0.5)
/// * `join_dr_routers` - Whether to join AllDRouters multicast group (224.0.0.6)
///
/// # Returns
/// A non-blocking raw socket configured for OSPF
#[cfg(unix)]
pub fn create_ospf_raw_socket(
    interface_addr: Ipv4Addr,
    join_all_routers: bool,
    join_dr_routers: bool,
) -> Result<Socket> {
    // Create raw IP socket for OSPF protocol (89)
    // Using unsafe to create SOCK_RAW socket via libc
    let socket = unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_RAW, IPPROTO_OSPF);
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Socket::from_raw_fd(fd)
    };

    // Set socket to non-blocking mode
    socket.set_nonblocking(true)?;

    // Set SO_REUSEADDR for address reuse
    socket.set_reuse_address(true)?;

    // Join AllSPFRouters multicast group (224.0.0.5) if requested
    if join_all_routers {
        socket.join_multicast_v4(&OSPF_ALL_SPF_ROUTERS, &interface_addr)?;
    }

    // Join AllDRouters multicast group (224.0.0.6) if requested (for DR/BDR)
    if join_dr_routers {
        socket.join_multicast_v4(&OSPF_ALL_DROUTERS, &interface_addr)?;
    }

    // Set multicast TTL to 1 (link-local only per RFC 2328)
    socket.set_multicast_ttl_v4(1)?;

    // Set multicast loopback to true (receive our own multicasts)
    socket.set_multicast_loop_v4(true)?;

    // Set multicast interface to the specified interface
    socket.set_multicast_if_v4(&interface_addr)?;

    Ok(socket)
}
