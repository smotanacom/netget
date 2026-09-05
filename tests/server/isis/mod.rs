//! IS-IS server tests

#[cfg(all(test, feature = "isis"))]
mod e2e_test;

#[cfg(all(test, feature = "isis"))]
mod hello_field_offsets_test;
