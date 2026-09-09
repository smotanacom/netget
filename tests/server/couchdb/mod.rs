//! CouchDB server E2E tests module

#[cfg(all(test, feature = "couchdb"))]
mod e2e_test;
#[cfg(all(test, feature = "couchdb"))]
mod llm_failure_test;
#[cfg(all(test, feature = "couchdb"))]
mod refusal_test;
