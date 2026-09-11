//! etcd E2E tests

#[cfg(all(test, feature = "etcd"))]
pub mod e2e_test;
#[cfg(all(test, feature = "etcd"))]
pub mod llm_failure_test;
#[cfg(all(test, feature = "etcd"))]
pub mod unanswered_request_test;
