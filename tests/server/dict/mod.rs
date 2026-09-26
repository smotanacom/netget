#[cfg(all(test, feature = "dict"))]
mod common;
#[cfg(all(test, feature = "dict"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "dict"))]
mod e2e_test;
#[cfg(all(test, feature = "dict"))]
mod llm_failure_test;
#[cfg(all(test, feature = "dict"))]
mod peer_inject_test;
#[cfg(all(test, feature = "dict"))]
mod real_client_test;
#[cfg(all(test, feature = "dict"))]
mod wire_test;
