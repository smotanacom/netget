//! VNC protocol E2E tests

#[cfg(all(test, feature = "vnc"))]
pub mod connection_bounds_test;
#[cfg(all(test, feature = "vnc"))]
pub mod inbound_limit_test;
#[cfg(all(test, feature = "vnc"))]
pub mod peer_inject_test;
#[cfg(all(test, feature = "vnc"))]
pub mod test;
