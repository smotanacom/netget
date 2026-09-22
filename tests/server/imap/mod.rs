//! IMAP E2E tests module

#[cfg(all(test, feature = "imap"))]
pub mod connection_bounds_test;
pub mod e2e_client_test;
#[cfg(all(test, feature = "imap"))]
pub mod line_limit_test;
#[cfg(all(test, feature = "imap"))]
pub mod literal_framing_test;
#[cfg(all(test, feature = "imap"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "imap"))]
pub mod peer_inject_test;
#[cfg(all(test, feature = "imap"))]
pub mod test;
