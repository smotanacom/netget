//! PyPI client tests

#[cfg(all(test, feature = "pypi"))]
pub mod e2e_test;

#[cfg(all(test, feature = "pypi"))]
pub mod command_channel_test;

#[cfg(all(test, feature = "pypi"))]
pub mod index_target_test;
