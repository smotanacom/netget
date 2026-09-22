//! WebRTC server E2E tests

#[cfg(all(test, feature = "webrtc"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "webrtc"))]
pub mod e2e_test;
