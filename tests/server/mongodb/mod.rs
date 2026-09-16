#[cfg(all(test, feature = "mongodb-server", feature = "mongodb"))]
pub mod e2e_test;
#[cfg(all(test, feature = "mongodb-server", feature = "mongodb"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "mongodb-server", feature = "mongodb"))]
pub mod required_fields_test;
// The peer-handle test needs only the server: it speaks raw OP_MSG rather than driving the
// `mongodb` client crate, so it is gated on `mongodb-server` alone.
#[cfg(all(test, feature = "mongodb-server"))]
pub mod peer_inject_test;
