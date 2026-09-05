//! OpenAPI server E2E tests

#[cfg(all(test, feature = "openapi"))]
pub mod e2e_test;

#[cfg(all(test, feature = "openapi"))]
pub mod e2e_route_matching_test;

#[cfg(all(test, feature = "openapi"))]
pub mod fail_closed_test;
