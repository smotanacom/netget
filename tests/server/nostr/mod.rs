#[cfg(all(test, feature = "nostr"))]
mod answer_with_test;
#[cfg(all(test, feature = "nostr"))]
mod common;
#[cfg(all(test, feature = "nostr"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "nostr"))]
mod e2e_test;
#[cfg(all(test, feature = "nostr"))]
mod llm_failure_test;
#[cfg(all(test, feature = "nostr"))]
mod peer_inject_test;
#[cfg(all(test, feature = "nostr"))]
mod real_client_test;
#[cfg(all(test, feature = "nostr"))]
mod wire_test;
