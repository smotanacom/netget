//! Receiving with the IP TTL, for RFC 5881 §5: a single-hop BFD packet whose TTL is not 255
//! came from more than one hop away and is discarded (the GTSM check). Tokio's `recv_from`
//! does not expose ancillary data, so this asks for `IP_RECVTTL` and reads it from `recvmsg`.
use std::io;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

/// Ask the kernel to report each datagram's TTL. IPv4 only; an IPv6 socket reports none.
#[cfg(unix)]
pub fn enable(socket: &UdpSocket) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if socket.local_addr()?.is_ipv6() {
        return Ok(());
    }
    let on: libc::c_int = 1;
    // SAFETY: a valid fd, and a pointer to an int of the length given.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_RECVTTL,
            &on as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn enable(_: &UdpSocket) -> io::Result<()> {
    Ok(())
}

/// One datagram, its source, and its TTL when the kernel reported one.
#[cfg(unix)]
pub async fn recv(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<u8>)> {
    use std::os::fd::AsRawFd;
    let fd = socket.as_raw_fd();
    socket
        .async_io(tokio::io::Interest::READABLE, || recv_raw(fd, buf))
        .await
}

#[cfg(unix)]
fn recv_raw(fd: i32, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, Option<u8>)> {
    // SAFETY: every pointer in the msghdr refers to a live local buffer of the stated length,
    // and the control messages are walked with the libc macros over that same msghdr.
    unsafe {
        let mut addr: libc::sockaddr_storage = std::mem::zeroed();
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let mut control = [0u64; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_name = &mut addr as *mut libc::sockaddr_storage as *mut libc::c_void;
        msg.msg_namelen = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = libc::recvmsg(fd, &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ttl = None;
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::IPPROTO_IP {
                let data = libc::CMSG_DATA(c);
                // Linux reports an int under IP_TTL; the BSDs a byte under IP_RECVTTL.
                #[cfg(any(target_os = "linux", target_os = "android"))]
                if (*c).cmsg_type == libc::IP_TTL {
                    ttl = Some(std::ptr::read_unaligned(data as *const libc::c_int) as u8);
                }
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                if (*c).cmsg_type == libc::IP_RECVTTL {
                    ttl = Some(*data);
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        let from = socket2::SockAddr::new(addr, msg.msg_namelen)
            .as_socket()
            .ok_or_else(|| io::Error::other("a datagram from a non-IP address"))?;
        Ok((n as usize, from, ttl))
    }
}

#[cfg(not(unix))]
pub async fn recv(
    socket: &UdpSocket,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<u8>)> {
    let (n, from) = socket.recv_from(buf).await?;
    Ok((n, from, None))
}
