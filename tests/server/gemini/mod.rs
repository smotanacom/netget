#[cfg(all(test, feature = "gemini"))]
mod common;
#[cfg(all(test, feature = "gemini"))]
mod connection_bounds_test;
#[cfg(all(test, feature = "gemini"))]
mod e2e_test;
#[cfg(all(test, feature = "gemini"))]
mod llm_failure_test;
#[cfg(all(test, feature = "gemini"))]
mod peer_inject_test;
#[cfg(all(test, feature = "gemini"))]
mod real_client_test;
#[cfg(all(test, feature = "gemini"))]
mod wire_test;
