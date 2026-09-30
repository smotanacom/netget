#[cfg(all(test, feature = "smb"))]
pub mod wire_util;

#[cfg(all(test, feature = "smb"))]
pub mod e2e_test;

#[cfg(all(test, feature = "smb"))]
pub mod e2e_llm_test;

#[cfg(all(test, feature = "smb"))]
pub mod llm_failure_test;

#[cfg(all(test, feature = "smb"))]
pub mod peer_inject_test;

#[cfg(all(test, feature = "smb"))]
pub mod inbound_limit_test;

#[cfg(all(test, feature = "smb"))]
pub mod header_layout_test;

#[cfg(all(test, feature = "smb"))]
pub mod real_client_test;

#[cfg(all(test, feature = "smb"))]
pub mod bounds_test;

#[cfg(all(test, feature = "smb"))]
pub mod failure_modes_test;
