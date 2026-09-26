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
// Deadlines only: raw sockets, no BSON driver, so the server feature alone is enough.
#[cfg(all(test, feature = "mongodb-server"))]
pub mod connection_bounds_test;
#[cfg(all(test, feature = "mongodb-server", feature = "mongodb"))]
pub mod real_client_test;
// Raw OP_MSG over a socket; decodes replies with `bson`, which `mongodb-server` already brings.
#[cfg(all(test, feature = "mongodb-server"))]
pub mod bson_depth_test;
