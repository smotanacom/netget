//! Utility modules

pub mod bencode;
pub mod sanitize;
pub mod save_load;
pub mod shutdown;
pub mod truncate;
pub mod wire_failure;

pub use shutdown::StopSignal;
pub use wire_failure::{prefixed_wire_failure_text, wire_failure_text, WireFailure};

pub use truncate::{
    truncate_for_llm, truncate_for_log, truncate_str, truncate_with_notice, truncate_with_suffix,
};
