#[cfg(all(test, feature = "elasticsearch"))]
pub mod e2e_test;
#[cfg(all(test, feature = "elasticsearch"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "elasticsearch"))]
pub mod refusal_test;

#[cfg(all(test, feature = "elasticsearch"))]
mod connection_bounds_test;
