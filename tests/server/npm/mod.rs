//! NPM registry protocol E2E tests

#[cfg(all(test, feature = "npm"))]
mod e2e_test;

#[cfg(all(test, feature = "npm"))]
mod status_range_test;

#[cfg(all(test, feature = "npm"))]
mod decision_tag_test;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "npm"))]
mod connection_bounds_test;
