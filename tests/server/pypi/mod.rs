//! PyPI protocol tests

#[cfg(all(test, feature = "pypi"))]
pub mod e2e_test;

#[cfg(all(test, feature = "pypi"))]
pub mod e2e_test_mocked;

/// The first-byte deadline, the idle deadline and the connection cap, from the wire.
#[cfg(all(test, feature = "pypi"))]
mod connection_bounds_test;
