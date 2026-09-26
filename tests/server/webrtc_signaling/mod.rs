//! WebRTC Signaling server E2E tests

#[cfg(all(test, feature = "webrtc"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "webrtc"))]
pub mod e2e_test;
#[cfg(all(test, feature = "webrtc"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "webrtc"))]
pub mod relay_abuse_test;
#[cfg(all(test, feature = "webrtc"))]
mod inbound_limit_test;
